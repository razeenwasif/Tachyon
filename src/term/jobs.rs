//! Job IDs in command output: a detached Nexus job, or a Slurm batch job.
//! Seeing one, Tachyon offers to follow it in a tab of its own.

/// The kinds of job Tachyon knows how to follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    /// The home cluster's coordinator: `nexus cpu/gpu ... --detach` prints
    /// `j-1a2b3c4d`, and `nexus logs` follows it.
    Nexus,
    /// `sbatch` prints `Submitted batch job 4821337`.
    Slurm,
}

impl JobKind {
    pub fn name(self) -> &'static str {
        match self {
            JobKind::Nexus => "nexus",
            JobKind::Slurm => "slurm",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobRef {
    pub kind: JobKind,
    pub id: String,
    /// The ssh command the job was submitted through, when it was, so the
    /// tab following it can reach the same machine.
    pub via: Option<String>,
}

impl JobRef {
    /// How a tab following it is named: `nexus j-1a2b3c4d`.
    pub fn title(&self) -> String {
        format!("{} {}", self.kind.name(), self.id)
    }
}

/// A Nexus job id: `j-` and eight hex digits.
fn nexus_id(word: &str) -> bool {
    word.len() == 10 && word.starts_with("j-") && word[2..].bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// A job id in one line of a command's output. Nexus ids count only in the
/// output of a `nexus cpu` or `nexus gpu` submission, so a log that happens
/// to mention one doesn't offer a tab.
pub fn detect(line: &str, cmdline: Option<&str>) -> Option<(JobKind, String)> {
    if let Some(rest) = line.split("Submitted batch job ").nth(1) {
        let id: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !id.is_empty() {
            return Some((JobKind::Slurm, id));
        }
    }
    // Any part of a chained line: `cd proj && nexus gpu ...` counts.
    let submits = cmdline.is_some_and(|c| {
        c.split(['&', '|', ';']).any(|part| {
            let mut w = part.split_whitespace().skip_while(|w| w.contains('=') || *w == "command" || *w == "exec");
            w.next().is_some_and(|p| p.rsplit('/').next() == Some("nexus")) && matches!(w.next(), Some("cpu" | "gpu"))
        })
    });
    if submits {
        let word = line
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .find(|w| nexus_id(w))?;
        return Some((JobKind::Nexus, word.to_string()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nexus_ids_only_in_submissions() {
        let submit = Some("nexus gpu --gpu-mem 20 --mem 32 --detach python train.py");
        assert_eq!(detect("j-1a2b3c4d", submit), Some((JobKind::Nexus, "j-1a2b3c4d".into())));
        assert_eq!(detect("  j-00ff00aa", Some("nexus cpu --detach make")), Some((JobKind::Nexus, "j-00ff00aa".into())));
        assert_eq!(detect("→ forge · RTX 4090 · now: free", submit), None);
        assert_eq!(detect("j-1a2b3c4d", Some("nexus logs j-1a2b3c4d")), None, "not a submission");
        assert_eq!(detect("j-1a2b3c4d", Some("cat notes.txt")), None);
        assert!(detect("j-1a2b3c4d", Some("cd ~/ml/vit && nexus gpu --detach python train.py")).is_some());
        assert!(detect("j-1a2b3c4d", Some("clear; ~/.local/bin/nexus cpu --detach make")).is_some());
        assert_eq!(detect("j-1A2B3C4D", submit), None, "ids are lower-case hex");
        assert_eq!(detect("j-1a2b3c4d5", submit), None);
    }

    #[test]
    fn finds_slurm_submissions_anywhere() {
        assert_eq!(detect("Submitted batch job 4821337", None), Some((JobKind::Slurm, "4821337".into())));
        assert_eq!(detect("Submitted batch job 77 on cluster gpu", Some("ssh hpc")), Some((JobKind::Slurm, "77".into())));
        assert_eq!(detect("Submitted batch job", None), None);
    }
}
