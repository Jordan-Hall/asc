use std::fmt;

pub struct Counter {
    n: u32,
}

impl Counter {
    pub fn new() -> Self {
        Counter { n: 0 }
    }

    pub fn bump(&mut self) {
        self.n += 1;
        log("bump");
        log("finished");
        log("really");
    }

    pub fn get(&self) -> u32 {
        self.n
    }
}
