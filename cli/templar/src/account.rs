//! Self-password changes use the owned host writer; no secret enters argv, JSON or history.
use std::{fs::{File, OpenOptions}, io::{Read, Write}};
use anyhow::{Context, Result};
use rustix::termios::{OptionalActions, Termios};
use zeroize::{Zeroize, Zeroizing};

const MAX_PASSWORD: usize = 1024;

struct PasswordTty {
    file: File,
    saved: Termios,
}

impl PasswordTty {
    fn open() -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/tty")
            .context("password entry requires a controlling terminal")?;
        let saved = rustix::termios::tcgetattr(&file)?;
        let mut raw = saved.clone();
        // Read controls ourselves: Ctrl-C cancels and runs Drop instead of terminating
        // while the terminal still has echo disabled.
        raw.make_raw();
        rustix::termios::tcsetattr(&file, OptionalActions::Now, &raw)?;
        Ok(Self { file, saved })
    }

    fn read(&mut self, prompt: &str, secret: &mut [u8; MAX_PASSWORD]) -> Result<usize> {
        self.file.write_all(prompt.as_bytes())?;
        self.file.flush()?;
        let mut length = 0;
        loop {
            let mut byte = [0];
            self.file.read_exact(&mut byte).context("reading password from terminal")?;
            match byte[0] {
                b'\n' | b'\r' => {
                    self.file.write_all(b"\r\n")?;
                    std::str::from_utf8(&secret[..length]).context("password must be valid UTF-8")?;
                    anyhow::ensure!(length != 0, "password must not be empty");
                    return Ok(length);
                }
                3 | 4 | 27 => anyhow::bail!("password entry cancelled"),
                8 | 127 => {
                    if length != 0 {
                        length -= 1;
                        while length != 0 && secret[length] & 0xc0 == 0x80 { length -= 1; }
                        secret[length..].zeroize();
                    }
                }
                21 => { secret.zeroize(); length = 0; }
                value if value < 32 => anyhow::bail!("password must not contain control characters"),
                value => {
                    anyhow::ensure!(length < secret.len(), "password exceeds {MAX_PASSWORD} bytes");
                    secret[length] = value;
                    length += 1;
                }
            }
        }
    }
}

impl Drop for PasswordTty {
    fn drop(&mut self) {
        if let Err(error) = rustix::termios::tcsetattr(&self.file, OptionalActions::Now, &self.saved) {
            eprintln!("templar: could not restore terminal settings: {error}");
        }
    }
}


pub async fn password() -> Result<()> {
    let uid = rustix::process::getuid();
    anyhow::ensure!(uid.as_raw() >= 1000 && uid == rustix::process::geteuid(),
        "run as your own login user, without sudo");
    let connection = zbus::connection::Builder::system()?
        .method_timeout(std::time::Duration::from_secs(360)).build().await.context("host system bus")?;
    let fd = {
        let mut tty = PasswordTty::open()?;
        let mut first = Zeroizing::new([0; MAX_PASSWORD]);
        let mut second = Zeroizing::new([0; MAX_PASSWORD]);
        let first_length = tty.read("New password: ", &mut first)?;
        let second_length = tty.read("Confirm password: ", &mut second)?;
        anyhow::ensure!(first[..first_length] == second[..second_length], "passwords do not match");
        let password = std::str::from_utf8(&first[..first_length]).context("password must be valid UTF-8")?;
        templar_client::secret_fd::seal_utf8(password, MAX_PASSWORD)?
    };
    let proxy = templar_client::proxies::HostAccountsProxy::new(&connection).await?;
    proxy.change_self_password(zbus::zvariant::OwnedFd::from(fd)).await
        .context("owned account writer refused the password change")?;
    println!("Password changed for the current and rollback roots.");
    Ok(())
}

