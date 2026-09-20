//! Kind isolation for legacy launch arguments. These checks bind user-supplied
//! argv to one CLI kind; they do not certify vendor flags or protocol support.
use anyhow::{Result,ensure};
pub mod profiles;
pub mod probe;
pub mod resolve;
pub fn arguments<'a>(kind:&str,bound_kind:Option<&str>,args:&'a [String],setting:&str)->Result<&'a [String]> {
    valid_kind(kind)?;
    if let Some(bound)=bound_kind {valid_kind(bound)?;}
    ensure!(args.len()<=128&&args.iter().map(String::len).sum::<usize>()<=65_536&&!args.iter().any(|a|a.contains('\0')),"{setting} exceeds argument limits or contains NUL");
    if !args.is_empty() {
        let bound=bound_kind.ok_or_else(||anyhow::anyhow!("{setting} is not bound to an agent kind; set {setting}_kind to the kind these existing arguments were written for before launching"))?;
        ensure!(bound==kind,"{setting} belongs to agent kind `{bound}`, not requested kind `{kind}`; configure arguments for the requested kind explicitly");
    }
    Ok(args)
}
fn valid_kind(kind:&str)->Result<()> {ensure!(!kind.is_empty()&&kind.len()<=64&&kind.as_bytes()[0].is_ascii_alphabetic()&&kind.bytes().all(|c|c.is_ascii_alphanumeric()||b"_-".contains(&c)),"invalid agent kind identifier");Ok(())}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_defaults_allow_mixed_kinds_but_flags_require_exact_binding() {
        for kind in ["claude","codex","devin","muse","grok"] {assert!(arguments(kind,None,&[],"thread_agent_args").is_ok());}
        let args=vec!["--vendor-option".into(),"literal value".into()];assert_eq!(arguments("claude",Some("claude"),&args,"thread_agent_args").unwrap(),args);
        assert!(arguments("claude",None,&args,"thread_agent_args").is_err());assert!(arguments("codex",Some("claude"),&args,"thread_agent_args").is_err());assert!(arguments("--kind",None,&[],"thread_agent_args").is_err());assert!(arguments("claude",Some("claude"),&["bad\0argument".into()],"thread_agent_args").is_err());
    }
}
