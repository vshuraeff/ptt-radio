//! --launch: the companion starts the game itself, so a Steam launch option
//! (`--launch %command%`) brings both up together. the companion runs while the
//! game lives, shuts down as usual when it exits and passes on its exit status.

use std::ffi::{OsStr, OsString};
use std::process::{Child, Command, ExitCode, ExitStatus};

const LUA_UDP: &str = "--enable-lua-udp";

/// the game command line, with `--enable-lua-udp <game_port>` appended unless it has the flag
fn command(argv: &[OsString], game_port: u16) -> Command {
    let (program, args) = argv.split_first().expect("clap requires a game command");
    let mut command = Command::new(program);
    command.args(args);
    if !args.iter().any(|arg| names_lua_udp(arg)) {
        command.arg(LUA_UDP).arg(game_port.to_string());
    }
    command
}

fn names_lua_udp(arg: &OsStr) -> bool {
    arg == LUA_UDP
        || arg
            .to_str()
            .is_some_and(|arg| arg.starts_with("--enable-lua-udp="))
}

pub fn spawn(argv: &[OsString], game_port: u16) -> Result<Child, String> {
    command(argv, game_port).spawn().map_err(|err| {
        format!(
            "--launch: cannot start {}: {err}",
            argv[0].to_string_lossy()
        )
    })
}

/// the game without the companion, for a companion that has just removed itself
#[cfg(feature = "self-update")]
pub fn run_alone(argv: &[OsString], game_port: u16) -> ExitCode {
    match spawn(argv, game_port).and_then(|mut child| child.wait().map_err(|e| e.to_string())) {
        Ok(status) => exit_code(status),
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// the game's own code when it fits an exit code, 1 otherwise (a signal, a large windows code)
pub fn exit_code(status: ExitStatus) -> ExitCode {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(command: &Command) -> Vec<&OsStr> {
        command.get_args().collect()
    }

    #[test]
    fn appends_the_lua_udp_port_only_when_the_game_lacks_it() {
        let argv = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        let plain = command(&argv(&["/games/factorio", "--mp-connect", "host"]), 34556);
        assert_eq!(plain.get_program(), "/games/factorio");
        assert_eq!(
            args(&plain),
            ["--mp-connect", "host", "--enable-lua-udp", "34556"]
        );
        for own in [
            &["factorio", "--enable-lua-udp", "40000"][..],
            &["factorio", "--enable-lua-udp=40000"],
        ] {
            assert_eq!(args(&command(&argv(own), 34556)), own[1..]);
        }
        assert_eq!(
            args(&command(&argv(&["factorio"]), 7)),
            ["--enable-lua-udp", "7"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_stub_game_gets_the_port_and_its_exit_status_is_passed_on() {
        let dir = std::env::temp_dir().join(format!("ptt-radio-launch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("argv");
        let script = format!("printf '%s\\n' \"$@\" > '{}'; exit 3", out.display());
        let argv: Vec<OsString> = ["/bin/sh", "-c", &script, "game"]
            .iter()
            .map(OsString::from)
            .collect();
        let status = spawn(&argv, 34600).unwrap().wait().unwrap();
        assert_eq!(status.code(), Some(3));
        assert_eq!(exit_code(status), ExitCode::from(3));
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "--enable-lua-udp\n34600\n"
        );
        let missing = [OsString::from("/nonexistent/ptt-radio-test-game")];
        assert!(spawn(&missing, 1).unwrap_err().contains("cannot start"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_signalled_game_maps_to_failure() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(exit_code(ExitStatus::from_raw(9)), ExitCode::FAILURE);
        assert_eq!(exit_code(ExitStatus::from_raw(0)), ExitCode::SUCCESS);
    }
}
