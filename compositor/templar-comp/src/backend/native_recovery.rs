//! The compositor never writes a shared crash marker itself. Hostd authenticates the
//! installed compositor and seat and owns the marker, including across greeter/user UIDs.
#[derive(Clone, Debug)]
pub struct Lease {
    connection: zbus::Connection,
    seat: String,
    lease: String,
    pub notice: String,
}
impl Lease {
    pub async fn start(seat: &str) -> anyhow::Result<Self> {
        let connection = zbus::Connection::system().await?;
        let proxy = zbus::Proxy::new(
            &connection,
            "org.templaros.Host",
            "/org/templaros/Host",
            "org.templaros.Host.DisplayRecovery1",
        )
        .await?;
        let (lease, notice): (String, String) = proxy.call("Start", &(seat,)).await?;
        Ok(Self {
            connection,
            seat: seat.into(),
            lease,
            notice,
        })
    }
    pub async fn begin(&self, gpu: &str) -> anyhow::Result<()> {
        let proxy = zbus::Proxy::new(
            &self.connection,
            "org.templaros.Host",
            "/org/templaros/Host",
            "org.templaros.Host.DisplayRecovery1",
        )
        .await?;
        proxy
            .call::<_, _, ()>(
                "BeginSecondary",
                &(self.seat.as_str(), self.lease.as_str(), gpu),
            )
            .await?;
        Ok(())
    }
    pub async fn presented(&self, gpu: &str) -> anyhow::Result<()> {
        let proxy = zbus::Proxy::new(
            &self.connection,
            "org.templaros.Host",
            "/org/templaros/Host",
            "org.templaros.Host.DisplayRecovery1",
        )
        .await?;
        proxy
            .call::<_, _, ()>(
                "FirstPresented",
                &(self.seat.as_str(), self.lease.as_str(), gpu),
            )
            .await?;
        Ok(())
    }
    pub async fn acquire(&self, gpu: &str) -> anyhow::Result<std::os::fd::OwnedFd> {
        let proxy = zbus::Proxy::new(
            &self.connection,
            "org.templaros.Host",
            "/org/templaros/Host",
            "org.templaros.Host.DisplayRecovery1",
        )
        .await?;
        let fd: zbus::zvariant::OwnedFd = proxy
            .call(
                "AcquireSecondary",
                &(self.seat.as_str(), self.lease.as_str(), gpu),
            )
            .await?;
        Ok(fd.into())
    }
    pub async fn resume(&self, gpu: &str) -> anyhow::Result<()> {
        self.begin(gpu).await?;
        let proxy = zbus::Proxy::new(
            &self.connection,
            "org.templaros.Host",
            "/org/templaros/Host",
            "org.templaros.Host.DisplayRecovery1",
        )
        .await?;
        proxy
            .call::<_, _, ()>(
                "ResumeSecondary",
                &(self.seat.as_str(), self.lease.as_str(), gpu),
            )
            .await?;
        Ok(())
    }
    pub async fn close(&self, gpu: &str) -> anyhow::Result<()> {
        let proxy = zbus::Proxy::new(
            &self.connection,
            "org.templaros.Host",
            "/org/templaros/Host",
            "org.templaros.Host.DisplayRecovery1",
        )
        .await?;
        proxy
            .call::<_, _, ()>(
                "CloseSecondary",
                &(self.seat.as_str(), self.lease.as_str(), gpu),
            )
            .await?;
        Ok(())
    }
}
