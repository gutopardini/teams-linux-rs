"""Install this checkout's launcher and icon for GNOME and Resources."""
from pathlib import Path
import os
import shutil
import subprocess
import sys

APP_ID = "io.github.gutopardini.TeamsLinux"
ROOT = Path(__file__).resolve().parent.parent
DATA = Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local/share"))
BINARY = ROOT / "target/release/teams-linux-rs"


def main():
    if not BINARY.is_file():
        raise SystemExit("Build first: cargo build --release")
    applications = DATA / "applications"
    applications.mkdir(parents=True, exist_ok=True)
    icon_root = DATA / "icons/hicolor"
    for directory, filename in [("scalable", "teams-linux-rs.svg"), ("256x256", "teams-linux-rs.png")]:
        dest = icon_root / directory / "apps"
        dest.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / "assets" / filename, dest / (APP_ID + Path(filename).suffix))
    executable = str(BINARY)
    if any(char.isspace() for char in executable):
        executable = '"' + executable.replace('\\', '\\\\').replace('"', '\\"') + '"'
    x11_test = "--x11-test" in sys.argv
    if "--trace-performance" in sys.argv or x11_test:
        executable += " --trace-performance"
    environment = "/usr/bin/env -u WAYLAND_DISPLAY " if x11_test else "/usr/bin/env "
    executable = environment + "TEAMS_LINUX_WEBRTC_AV=1 " + executable
    suffix = "-x11-test" if x11_test else ""
    desktop = applications / (APP_ID + suffix + ".desktop")
    name = "Teams Linux — teste X11" if x11_test else "Teams Linux"
    desktop.write_text(
        f"[Desktop Entry]\nType=Application\nName={name}\n"
        "Comment=Native client for Teams chats and calls\n"
        "Comment[pt_BR]=Cliente nativo para chats e chamadas do Teams\n"
        f"Exec={executable}\nTryExec={BINARY}\nPath={ROOT}\nIcon={APP_ID}\n"
        f"StartupWMClass={APP_ID}\nTerminal=false\nStartupNotify=false\n"
        "Categories=Network;Chat;InstantMessaging;\n"
    )
    subprocess.run(["desktop-file-validate", str(desktop)], check=True)
    if not x11_test:
        (applications / (APP_ID + "-webrtc-test.desktop")).unlink(missing_ok=True)
    subprocess.run(["update-desktop-database", str(applications)], check=True)
    subprocess.run(["gtk-update-icon-cache", "-f", "-t", str(icon_root)], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    print(f"Installed desktop ID: {APP_ID}")
    print(f"Launcher: {desktop}")
    # Resources also matches .desktop executable_name with the process executable.
    for proc in Path("/proc").iterdir():
        if not proc.name.isdigit():
            continue
        try:
            first_arg = (proc / "cmdline").read_bytes().split(b"\0", 1)[0].decode()
            if Path(first_arg).name == BINARY.name:
                print(f"Resources executable match: Teams Linux, PID {proc.name}")
        except (OSError, UnicodeError):
            pass


if __name__ == "__main__":
    main()
