//! An owned subprocess survives GUI death. Its own monotonic deadline restores real raw RandR state.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{io::{BufRead, BufReader, Read, Write}, process::{Child, ChildStdin, ChildStdout, Command, Stdio}, sync::mpsc, time::{Duration, Instant}};
use templar_types::{display::{DisplayObservation, DisplayProfile, PreviewKind, PreviewObservation, PreviewProof}, settings::Revision};
use uuid::Uuid;
use crate::randr::{Randr, Snapshot};
const DEADLINE: Duration = Duration::from_secs(15);
const MAX_FRAME: usize = templar_types::compositor_control::MAX_CONTROL_FRAME;
#[derive(Serialize, Deserialize)]
struct Begin { expected: Revision, fingerprint: [u8;32], profile: DisplayProfile }
#[derive(Serialize, Deserialize)]
enum Request { Observe, Confirm { expected: Revision, token: Uuid }, Revert { token: Uuid } }
#[derive(Serialize, Deserialize)]
struct Reply { result: Result<DisplayObservation,String>, fingerprint: Option<[u8;32]>, finished: bool, confirmed: bool }
fn write_frame(writer:&mut impl Write,value:&impl Serialize)->Result<()> { let bytes=serde_json::to_vec(value)?; ensure!(bytes.len()<=MAX_FRAME,"Preview frame exceeds bounded protocol"); writer.write_all(&bytes)?; writer.write_all(b"\n")?; writer.flush()?; Ok(()) }
fn read_frame<T:serde::de::DeserializeOwned>(reader:&mut impl BufRead)->Result<Option<T>> { let mut bytes=Vec::new(); let count=reader.take((MAX_FRAME+1) as u64).read_until(b'\n',&mut bytes)?; if count==0 {return Ok(None);} ensure!(bytes.len()<=MAX_FRAME && bytes.last()==Some(&b'\n'),"Oversized or incomplete preview frame"); Ok(Some(serde_json::from_slice(&bytes)?)) }
pub struct Preview { child: Option<Child>, stdin: Option<ChildStdin>, stdout: BufReader<ChildStdout>, pub token: Uuid, finished: bool, confirmed: bool }
impl Preview {
    pub fn begin(before:&Snapshot,profile:DisplayProfile)->Result<(Self,DisplayObservation,[u8;32])> {
        let mut child=Command::new(std::env::current_exe()?).arg("--rollback-helper").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().context("Start independent actual RandR rollback owner")?;
        let mut stdin=child.stdin.take().context("Owned helper stdin unavailable")?; let stdout=child.stdout.take().context("Owned helper stdout unavailable")?;
        write_frame(&mut stdin,&Begin {expected:before.observation.revision,fingerprint:before.fingerprint,profile})?;
        let mut preview=Self {child:Some(child),stdin:Some(stdin),stdout:BufReader::new(stdout),token:Uuid::nil(),finished:false,confirmed:false};
        let reply:Reply=read_frame(&mut preview.stdout)?.context("Rollback owner exited before target readback")?;
        if reply.finished {preview.reap()?;}
        let observation=reply.result.map_err(anyhow::Error::msg)?; ensure!(!reply.finished,"Rollback owner did not retain preview ownership"); preview.token=observation.preview.as_ref().context("Owner omitted preview token")?.token;
        Ok((preview,observation,reply.fingerprint.context("Owner omitted readback fingerprint")?))
    }
    pub fn observe(&mut self)->Result<(DisplayObservation,[u8;32],bool)> { self.request(Request::Observe) }
    pub fn confirm(&mut self,expected:Revision,token:Uuid)->Result<(DisplayObservation,[u8;32],bool)> {
        let result=self.request(Request::Confirm {expected,token})?;
        ensure!(self.confirmed,"The owner reverted/expired instead of confirming this target; actual state must be read again");
        Ok(result)
    }
    pub fn revert(&mut self,token:Uuid)->Result<(DisplayObservation,[u8;32],bool)> {self.request(Request::Revert {token})}
    fn request(&mut self,request:Request)->Result<(DisplayObservation,[u8;32],bool)> {
        if let Some(stdin)=&mut self.stdin { let _=write_frame(stdin,&request); }
        let reply = match read_frame::<Reply>(&mut self.stdout) {
            Ok(Some(reply)) => reply,
            other => {
                if self.child.as_mut().map(|child| child.try_wait()).transpose()?.flatten().is_some() { self.reap()?; }
                return match other { Ok(None) => Err(anyhow::anyhow!("Rollback owner disconnected; inspect actual RandR state rather than assuming it restored")), Err(error) => Err(error), Ok(Some(_)) => unreachable!() };
            }
        };
        self.confirmed=reply.confirmed;
        if reply.finished {self.reap()?;}
        let result=reply.result.map_err(anyhow::Error::msg)?;
        Ok((result,reply.fingerprint.context("Owner omitted readback fingerprint")?,reply.finished))
    }
    pub fn finished(&self)->bool {self.finished}
    fn reap(&mut self)->Result<()> {self.stdin.take();if let Some(mut child)=self.child.take(){child.wait().context("Reap the exact owned rollback process")?;}self.finished=true;Ok(())}
}
impl Drop for Preview {
    fn drop(&mut self) {
        self.stdin.take();
        if let Some(mut child)=self.child.take() {
            // EOF tells the owner to restore. Reap only that process; never kill it first.
            let pid=child.id();
            if let Err(error)=std::thread::Builder::new().name("owned-randr-preview-reaper".into()).spawn(move||{let _=child.wait();}) {
                eprintln!("Owned rollback reaper thread unavailable: {error}; waiting for this exact spawned child, never killing it");
                // The failed spawn dropped the Child handle, not the running process. This is
                // the PID returned by our own spawn and waitpid accepts only our own children.
                #[allow(unsafe_code)]
                loop {
                    let mut status=0;
                    let waited=unsafe{libc::waitpid(pid.try_into().expect("Linux spawned PID fits pid_t"),&mut status,0)};
                    if waited>=0 {break;}
                    let error=std::io::Error::last_os_error();
                    if error.kind()!=std::io::ErrorKind::Interrupted {eprintln!("Exact owned child reap failed: {error}");break;}
                }
            }
        }
    }
}
fn projected(snapshot:&Snapshot,token:Uuid,deadline:Instant,rollback_failure:Option<String>)->DisplayObservation {
    let mut observation=snapshot.observation.clone();
    observation.preview=Some(PreviewObservation {token,kind:PreviewKind::Interactive,proof:PreviewProof::RandRReadback,confirmation_remaining_ms:rollback_failure.is_none().then(||deadline.saturating_duration_since(Instant::now()).as_millis().min(u128::from(u64::MAX)) as u64),confirmed:false,rollback_failure}); observation
}
fn send_reply(writer:&mut impl Write,result:Result<DisplayObservation>,fingerprint:Option<[u8;32]>,finished:bool,confirmed:bool)->Result<()> {write_frame(writer,&Reply {result:result.map_err(|error|format!("{error:#}")),fingerprint,finished,confirmed})}
struct RollbackFailure { fingerprint: Option<[u8;32]>, message: String }
fn restore(backend:&mut Randr,baseline:&Snapshot,writer:&mut impl Write)->Option<RollbackFailure> {
    match backend.rollback(baseline) {
        Ok(restored)=>{let _=send_reply(writer,Ok(restored.observation),Some(restored.fingerprint),true,false);None},
        Err(error)=>Some(RollbackFailure {fingerprint:backend.snapshot().ok().map(|actual|actual.fingerprint),message:format!("Actual RandR rollback failed: {error:#}")}),
    }
}
pub fn helper()->Result<()> {
    let mut initial=BufReader::new(std::io::stdin());let begin:Begin=read_frame(&mut initial)?.context("GUI ended before any native effect")?;
    let mut backend=Randr::connect(begin.expected.owner_epoch,begin.expected.generation)?;
    let baseline=backend.snapshot()?;ensure!(baseline.fingerprint==begin.fingerprint,"The actual RandR topology/configuration changed before owned preview");
    let target=begin.profile;let token=Uuid::new_v4();let mut output=std::io::stdout();
    // Establish the bounded input owner before any native effect; a thread-creation failure
    // must not abandon a partially applied display transaction.
    let (sender,receiver)=mpsc::sync_channel(8);
    std::thread::Builder::new().name("owned-randr-preview-input".into()).spawn(move||{
        loop {match read_frame::<Request>(&mut initial) {Ok(Some(request))=>if sender.send(Ok(request)).is_err(){break;},Ok(None)=>break,Err(error)=>{let _=sender.send(Err(error));break;}}}
    })?;
    let (applied,mut rollback_failure)=match backend.apply(&baseline,begin.expected,&target) {
        Ok(applied)=>(Some(applied),None),
        Err(error)=>match backend.rollback(&baseline) {
            Ok(restored)=>{let _=send_reply(&mut output,Err(error.context("Preview application failed; original raw state restored and read back")),Some(restored.fingerprint),true,false);return Ok(());},
            Err(rollback)=>{
                let actual=backend.snapshot().ok();
                let failure=RollbackFailure {fingerprint:actual.as_ref().map(|actual|actual.fingerprint),message:format!("Target was not applied/read back: {error:#}; original-state rollback also failed: {rollback:#}")};
                (actual,Some(failure))
            },
        },
    };
    // Actual server readback only; never native KMS/page-flip/lock coverage.
    let mut deadline=Instant::now()+DEADLINE;
    let delivered=match &applied {
        Some(applied)=>send_reply(&mut output,Ok(projected(applied,token,deadline,rollback_failure.as_ref().map(|failure|failure.message.clone()))),Some(applied.fingerprint),false,false),
        None=>send_reply(&mut output,Err(anyhow::anyhow!("Display application and rollback failed, and actual readback is unavailable; independent rollback ownership is retained")),None,false,false),
    }.is_ok();
    let mut disconnected=!delivered;
    loop {
        let now=Instant::now();
        if rollback_failure.is_none() && (disconnected || now>=deadline) {
            rollback_failure=restore(&mut backend,&baseline,&mut output);
            if rollback_failure.is_none(){return Ok(());}
        }
        let request=if disconnected {
            std::thread::sleep(Duration::from_millis(500));None
        }else{
            match receiver.recv_timeout(if rollback_failure.is_some(){Duration::from_millis(500)}else{deadline.saturating_duration_since(now).min(Duration::from_millis(500))}) {
                Ok(Ok(request))=>Some(request),
                Ok(Err(error))=>{eprintln!("Owned preview input ended: {error:#}; restoring rather than exiting");disconnected=true;None},
                Err(mpsc::RecvTimeoutError::Timeout)=>None,
                Err(mpsc::RecvTimeoutError::Disconnected)=>{disconnected=true;None},
            }
        };
        let mut actual=match backend.snapshot() {
            Ok(actual)=>actual,
            Err(error)=>{if request.is_some(){let _=send_reply(&mut output,Err(error.context("Actual server readback unavailable; rollback ownership retained")),None,false,false);}std::thread::sleep(Duration::from_millis(500));continue;},
        };
        if rollback_failure.is_none() && applied.as_ref().is_some_and(|applied| applied.fingerprint != actual.fingerprint) {
            rollback_failure=restore(&mut backend,&baseline,&mut output);
            if rollback_failure.is_none(){return Ok(());}
            actual=match backend.snapshot() {Ok(actual)=>actual,Err(error)=>{if request.is_some(){let _=send_reply(&mut output,Err(error.context("Actual readback unavailable after failed rollback; ownership retained")),None,false,false);}continue;}};
        }
        // Retry failed effects only after a real native configuration/topology recovery.
        if rollback_failure.as_ref().is_some_and(|failure|failure.fingerprint!=Some(actual.fingerprint)) {
            rollback_failure=restore(&mut backend,&baseline,&mut output);
            if rollback_failure.is_none(){return Ok(());}
            actual=match backend.snapshot() {Ok(actual)=>actual,Err(error)=>{if request.is_some(){let _=send_reply(&mut output,Err(error.context("Actual readback unavailable after failed recovery rollback; ownership retained")),None,false,false);}continue;}};
        }
        match request {
            Some(Request::Observe)=>{let _=send_reply(&mut output,Ok(projected(&actual,token,deadline,rollback_failure.as_ref().map(|failure|failure.message.clone()))),Some(actual.fingerprint),false,false);},
            Some(Request::Confirm {expected,token:requested})=>{
                let checked=(||->Result<()>{ensure!(requested==token && expected==actual.observation.revision && Instant::now()<deadline && rollback_failure.is_none(),"Preview token/owner/revision expired or changed");backend.verify(&actual,&target)})();
                if let Err(error)=checked {let _=send_reply(&mut output,Err(error),Some(actual.fingerprint),false,false);continue;}
                let _=send_reply(&mut output,Ok(actual.observation),Some(actual.fingerprint),true,true);return Ok(());
            },
            Some(Request::Revert {token:requested})=>{
                if requested!=token {let _=send_reply(&mut output,Err(anyhow::anyhow!("Wrong owned preview token")),Some(actual.fingerprint),false,false);continue;}
                deadline=Instant::now();rollback_failure=restore(&mut backend,&baseline,&mut output);
                if rollback_failure.is_none(){return Ok(());}
                match backend.snapshot() {
                    Ok(fresh)=>{let _=send_reply(&mut output,Ok(projected(&fresh,token,deadline,rollback_failure.as_ref().map(|failure|failure.message.clone()))),Some(fresh.fingerprint),false,false);},
                    Err(error)=>{let _=send_reply(&mut output,Err(error.context("Rollback failed and actual readback is unavailable; independent ownership retained")),None,false,false);},
                }
            },
            None=>{},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn bounded_pipe_protocol_rejects_partial_and_oversized_messages() {let mut partial=BufReader::new(&b"{\"Observe\":null}"[..]);assert!(read_frame::<Request>(&mut partial).is_err());let bytes=vec![b'a';MAX_FRAME+2];let mut oversized=BufReader::new(bytes.as_slice());assert!(read_frame::<Request>(&mut oversized).is_err());}
}
