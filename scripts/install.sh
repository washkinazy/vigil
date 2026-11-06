#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
config_dir=${XDG_CONFIG_HOME:-"$HOME/.config"}
bin_dir="$HOME/.local/bin"
install -d "$bin_dir" "$config_dir/systemd/user" "$config_dir/vigil"
# Replace executables atomically, including while a previous version is running.
for name in vigild vigilctl; do
    install -m 755 "target/release/$name" "$bin_dir/.$name.new"
    mv -f "$bin_dir/.$name.new" "$bin_dir/$name"
done
install -m 644 packaging/vigil.service "$config_dir/systemd/user/vigil.service"
if [ ! -e "$config_dir/vigil/config.toml" ]; then
    install -m 644 config.example.toml "$config_dir/vigil/config.toml"
fi
systemctl --user daemon-reload
printf '%s\n' 'Installed Vigil. Existing configuration preserved. Start with: systemctl --user start vigil.service'
