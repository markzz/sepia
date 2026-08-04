use std::path::PathBuf;

#[derive(Default)]
pub struct NavigationHistory {
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
}

impl NavigationHistory {
    pub fn record(&mut self, leaving: PathBuf) {
        self.back.push(leaving);
        self.forward.clear();
    }

    pub fn go_back(&mut self, current: PathBuf) -> Option<PathBuf> {
        let target = self.back.pop()?;
        self.forward.push(current);
        Some(target)
    }

    pub fn go_forward(&mut self, current: PathBuf) -> Option<PathBuf> {
        let target = self.forward.pop()?;
        self.back.push(current);
        Some(target)
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}
