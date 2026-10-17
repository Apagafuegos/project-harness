//! Durability barriers with deterministic failure injection in unit tests.
use super::error::Result;
use std::{fs::File, io::Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Point {
    BeforeWrite,
    PartialWrite,
    Written,
    Flushed,
    Synced,
    BeforeAck,
    HeaderWritten,
    HeaderSynced,
    SessionDirectorySynced,
    ArtifactWritten,
    ArtifactSynced,
    ArtifactRenamed,
    ArtifactDirectorySynced,
    TailTruncated,
    TailSynced,
    ReopenSynced,
}

#[derive(Clone, Default, Debug)]
pub(crate) struct Io {
    #[cfg(test)]
    pub test: std::sync::Arc<std::sync::Mutex<TestIo>>,
}
#[cfg(test)]
#[derive(Default, Debug)]
pub(crate) struct TestIo {
    pub fail: Option<Point>,
    pub trace: Vec<Point>,
}
impl Io {
    pub fn check(&self, point: Point) -> Result<()> {
        #[cfg(not(test))]
        let _ = point;
        #[cfg(test)]
        {
            let mut state = self.test.lock().unwrap();
            state.trace.push(point);
            if state.fail == Some(point) {
                state.fail = None;
                return Err(std::io::Error::other("injected storage failure").into());
            }
        }
        Ok(())
    }
    pub fn append(&self, file: &mut File, bytes: &[u8]) -> Result<()> {
        self.check(Point::BeforeWrite)?;
        // Exercise a real short append in tests before reporting an I/O failure.
        #[cfg(test)]
        if self.test.lock().unwrap().fail == Some(Point::PartialWrite) {
            file.write_all(&bytes[..bytes.len() / 2])?;
        }
        self.check(Point::PartialWrite)?;
        file.write_all(bytes)?;
        self.check(Point::Written)?;
        file.flush()?;
        self.check(Point::Flushed)?;
        file.sync_all()?;
        self.check(Point::Synced)?;
        self.check(Point::BeforeAck)
    }
}
