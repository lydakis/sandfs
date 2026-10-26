use std::fmt;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::nfsserve::transaction_tracker::TransactionTracker;
use crate::nfsserve::vfs::NFSFileSystem;

#[derive(Clone)]
pub struct RPCContext {
    pub local_port: u16,
    pub client_addr: String,
    pub auth: crate::nfsserve::rpc::auth_unix,
    pub vfs: Arc<dyn NFSFileSystem + Send + Sync>,
    pub mount_signal: Option<mpsc::Sender<bool>>,
    pub export_name: Arc<String>,
    pub transaction_tracker: Arc<TransactionTracker>,
    pub filter: Option<Arc<dyn CallFilter>>,
}

/// Looks at each call, as its bytes came, before the server does.
pub trait CallFilter: Send + Sync {
    fn check(&self, call: &[u8]) -> Verdict;
}

/// What becomes of a call.
pub enum Verdict {
    /// The server answers it.
    Serve,
    /// The server answers it, and this changes the reply before it goes out.
    Correct(fn(&mut [u8])),
    /// This is the reply instead, and the server never sees the call.
    Reply(Vec<u8>),
}

impl fmt::Debug for RPCContext {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("RPCContext")
            .field("local_port", &self.local_port)
            .field("client_addr", &self.client_addr)
            .field("auth", &self.auth)
            .finish()
    }
}
