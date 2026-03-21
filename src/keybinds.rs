const ESC: u8 = 0x1B;

#[derive(Debug, Clone, Copy)]
pub enum Action {
    SwitchTab(usize),
}

pub struct Keybinds {
    bindings: Vec<(Vec<u8>, Action)>,
    pending: Vec<u8>,
    max_len: usize,
}

impl Keybinds {
    pub fn new() -> Self {
        let mut bindings = Vec::new();
        for digit in 1..=9u8 {
            bindings.push((vec![ESC, b'0' + digit], Action::SwitchTab(digit as usize)));
        }
        let max_len = bindings.iter().map(|(seq, _)| seq.len()).max().unwrap_or(0);
        Self { bindings, pending: Vec::new(), max_len }
    }

    /// Process input bytes. Returns (actions_to_dispatch, bytes_to_forward).
    pub fn feed(&mut self, input: &[u8]) -> (Vec<Action>, Vec<u8>) {
        let mut actions = Vec::new();
        let mut forward = Vec::new();

        for &byte in input {
            self.pending.push(byte);

            if let Some(action) = self.find_exact_match() {
                actions.push(action);
                self.pending.clear();
            } else if self.has_prefix_match() {
                if self.pending.len() >= self.max_len {
                    forward.extend_from_slice(&self.pending);
                    self.pending.clear();
                }
            } else {
                forward.extend_from_slice(&self.pending);
                self.pending.clear();
            }
        }

        (actions, forward)
    }

    fn find_exact_match(&self) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(seq, _)| *seq == self.pending)
            .map(|(_, action)| *action)
    }

    fn has_prefix_match(&self) -> bool {
        self.bindings
            .iter()
            .any(|(seq, _)| seq.len() > self.pending.len() && seq.starts_with(&self.pending))
    }
}
