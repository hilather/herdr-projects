    use super::{Cmd, Output, Runner};
    use anyhow::Result;
    use std::{path::{Path, PathBuf}, time::Duration};
    use std::cell::RefCell;

    type Matcher = Box<dyn Fn(&Cmd) -> bool>;

    /// A scripted runner: the first rule whose matcher accepts the command
    /// answers it. Every command is recorded, matched or not.
    #[derive(Default)]
    pub struct FakeRunner {
        rules: RefCell<Vec<(Matcher, Box<dyn Fn(&Cmd) -> Result<Output>>)>>,
        pub calls: RefCell<Vec<Cmd>>,
        /// (socket, request line) of every socket request.
        pub socket_requests: RefCell<Vec<(PathBuf, String)>>,
    }

    impl FakeRunner {
        pub fn new() -> Self {
            Self::default()
        }

        /// Answer commands whose display line contains `needle`.
        pub fn on(&self, needle: &str, output: Output) -> &Self {
            let needle = needle.to_string();
            self.rules.borrow_mut().push((
                Box::new(move |cmd| cmd.display().contains(&needle)),
                Box::new(move |_| Ok(output.clone())),
            ));
            self
        }

        pub fn on_fn(
            &self,
            matcher: impl Fn(&Cmd) -> bool + 'static,
            answer: impl Fn(&Cmd) -> Result<Output> + 'static,
        ) -> &Self {
            self.rules
                .borrow_mut()
                .push((Box::new(matcher), Box::new(answer)));
            self
        }

        pub fn count(&self, needle: &str) -> usize {
            self.calls
                .borrow()
                .iter()
                .filter(|cmd| cmd.display().contains(needle))
                .count()
        }
    }

    pub fn ok(stdout: &str) -> Output {
        Output {
            code: Some(0),
            stdout: stdout.to_string(),
            stdout_bytes: stdout.as_bytes().to_vec(),
            stdout_total_bytes: stdout.len() as u64,
            ..Output::default()
        }
    }

    pub fn fail(code: i32, stderr: &str) -> Output {
        Output {
            code: Some(code),
            stderr: stderr.to_string(),
            stderr_bytes: stderr.as_bytes().to_vec(),
            stderr_total_bytes: stderr.len() as u64,
            ..Output::default()
        }
    }

    pub fn timeout() -> Output {
        Output {
            timed_out: true,
            ..Output::default()
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, cmd: &Cmd) -> Result<Output> {
            self.calls.borrow_mut().push(cmd.clone());
            for (matcher, answer) in self.rules.borrow().iter() {
                if matcher(cmd) {
                    let mut output = answer(cmd)?;
                    if let Some((path, limit)) = &cmd.stdout_file {
                        use std::io::Write;
                        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
                        file.write_all(&output.stdout_bytes[..output.stdout_bytes.len().min(*limit)])?;
                        output.stdout_truncated |= output.stdout_bytes.len() > *limit;
                        output.stdout.clear();
                        output.stdout_bytes.clear();
                    }
                    return Ok(output);
                }
            }
            anyhow::bail!("FakeRunner: no rule for `{}`", cmd.display())
        }

        fn socket_request(&self, socket: &Path, line: &str, _timeout: Duration) -> Result<String> {
            self.socket_requests.borrow_mut().push((socket.to_path_buf(), line.to_string()));
            Ok(r#"{"id":"hp","result":{"type":"agent_view","active":true}}"#.to_string())
        }
    }
