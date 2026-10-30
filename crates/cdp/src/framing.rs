//! Incremental top-level envelope fields, including fields after an oversized payload.
#[derive(Default)]
pub(crate) struct Metadata {
    pub session: Option<String>,
    pub id: Option<u64>,
    depth: usize,
    string: bool,
    escaped: bool,
    token: Vec<u8>,
    key: String,
    expect_key: bool,
    number: String,
}
impl Metadata {
    pub fn feed(&mut self, byte: u8) {
        if self.string {
            if self.depth == 1 && self.token.len() < 4096 {
                self.token.push(byte);
            }
            if self.escaped {
                self.escaped = false;
                return;
            }
            if byte == b'\\' {
                self.escaped = true;
            } else if byte == b'"' {
                self.string = false;
                if self.depth == 1
                    && let Ok(value) = serde_json::from_slice::<String>(&self.token)
                {
                    if self.expect_key {
                        self.key = value;
                        self.expect_key = false;
                    } else if self.key == "sessionId" {
                        self.session = Some(value);
                    }
                }
                self.token.clear();
            }
            return;
        }
        if self.depth == 1 && self.key == "id" && byte.is_ascii_digit() && self.number.len() < 32 {
            self.number.push(byte as char);
        }
        match byte {
            b'"' => {
                self.string = true;
                self.token.clear();
                if self.depth == 1 {
                    self.token.push(byte);
                }
            }
            b'{' | b'[' => {
                self.depth += 1;
                if self.depth == 1 {
                    self.expect_key = true;
                }
            }
            b'}' | b']' | b',' => {
                if self.depth == 1 {
                    if !self.number.is_empty() {
                        self.id = self.number.parse().ok();
                        self.number.clear();
                    }
                    self.key.clear();
                    self.expect_key = true;
                }
                if byte != b',' {
                    self.depth = self.depth.saturating_sub(1);
                }
            }
            _ => {}
        }
    }
}
