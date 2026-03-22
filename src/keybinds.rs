const ESC: u8 = 0x1B;

#[derive(Debug, Clone, Copy)]
pub enum Action {
    SwitchTab(usize),
    Detach,
    ScrollPageUp,
    ScrollPageDown,
    NewPane,
    FocusDown,
    FocusUp,
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
        bindings.push((vec![ESC, b'd'], Action::Detach));
        bindings.push((vec![ESC, 0x0D], Action::NewPane));   // Alt+Enter
        bindings.push((vec![ESC, b'j'], Action::FocusDown));  // Alt+j
        bindings.push((vec![ESC, b'k'], Action::FocusUp));    // Alt+k
        // Shift+PageUp / Shift+PageDown
        bindings.push((vec![ESC, b'[', b'5', b';', b'2', b'~'], Action::ScrollPageUp));
        bindings.push((vec![ESC, b'[', b'6', b';', b'2', b'~'], Action::ScrollPageDown));
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
