use clap::Command;
use clap_mangen::Man;
use std::io;
use std::path::{Path, PathBuf};

pub fn cmd_man(cli_cmd: Command, out_dir: Option<PathBuf>) -> io::Result<()> {
    let out_dir = out_dir.unwrap_or_else(|| PathBuf::from("man"));
    std::fs::create_dir_all(&out_dir)?;

    write_manpage(&cli_cmd, &out_dir)?;

    for subcmd in cli_cmd.get_subcommands() {
        let subcmd_name: &'static str =
            Box::leak(format!("oxiarc-{}", subcmd.get_name()).into_boxed_str());
        let named = subcmd.clone().name(subcmd_name);
        write_manpage(&named, &out_dir)?;
    }

    println!("Man pages written to: {}", out_dir.display());
    Ok(())
}

fn write_manpage(cmd: &Command, out_dir: &Path) -> io::Result<()> {
    let name = cmd.get_name();
    let file_name = if name.starts_with("oxiarc") {
        format!("{name}.1")
    } else {
        format!("oxiarc-{name}.1")
    };
    let path = out_dir.join(&file_name);
    let mut file = std::fs::File::create(&path)?;
    Man::new(cmd.clone()).render(&mut file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn man_page_generates() -> Result<(), Box<dyn std::error::Error>> {
        use clap::CommandFactory;

        let dir = std::env::temp_dir().join(format!("oxiarc_man_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;

        let cmd = crate::Cli::command();
        cmd_man(cmd, Some(dir.clone()))?;

        let page = dir.join("oxiarc.1");
        assert!(page.exists(), "man page not generated at {page:?}");

        let content = std::fs::read_to_string(&page)?;
        assert!(
            content.contains(".TH") || content.contains("oxiarc"),
            "unexpected man page content"
        );

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
