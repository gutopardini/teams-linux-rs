#[derive(Clone, PartialEq, Eq)]
pub enum Error {
    Sdp(String),
    Engine,
}
impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Sdp(_) => "Sdp([REDACTED])",
            Self::Engine => "Engine",
        })
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Sdp(_) => "Invalid media description",
            Self::Engine => "WebRTC media error",
        })
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
