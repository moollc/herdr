//! Shared login argument validation. The paste command is handled separately.
#[derive(Default, Debug, PartialEq)]
pub(super) struct LoginArgs {
    pub(super) flag: String,
    pub(super) browser: Option<String>,
}
pub(super) fn parse_args(args: &[String]) -> Result<LoginArgs, ()> {
    let mut parsed = LoginArgs::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--browser" && parsed.browser.is_none() {
            let name = args
                .next()
                .filter(|name| !name.starts_with('-'))
                .ok_or(())?;
            parsed.browser = Some(name.clone());
        } else if ["--refresh", "--status", "--forget"].contains(&arg.as_str())
            && parsed.flag.is_empty()
        {
            parsed.flag = arg.clone();
        } else {
            return Err(());
        }
    }
    if parsed.browser.is_some() && ["--status", "--forget"].contains(&parsed.flag.as_str()) {
        return Err(());
    }
    Ok(parsed)
}
