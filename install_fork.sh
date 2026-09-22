#!/bin/sh
# Install the adv-voltage-xbar LACT fork system-wide and start it as the
# system daemon. Run from the LACT checkout as root:
#     sudo sh install_fork.sh
# Installs under /usr/local (binary, systemd unit, desktop entry, icons,
# polkit policy) so a future packaged `lact` in /usr would not overwrite it.
set -e
cd "$(dirname "$0")"
[ "$(id -u)" = 0 ] || { echo "run with sudo"; exit 1; }
[ -x target/release/lact ] || { echo "no release binary; run: cargo build -p lact --release"; exit 1; }
# Building a single crate (-p lact-gui / -p lact-daemon) refreshes only its
# rlib; `make install` copies target/release/lact, which then goes out stale.
stale=$(find lact-*/src -name '*.rs' -newer target/release/lact | head -1)
[ -z "$stale" ] || { echo "release binary is older than $stale; run: cargo build -p lact --release"; exit 1; }

echo "== stopping any running lact daemon =="
systemctl stop lactd 2>/dev/null || true
for p in $(pgrep -x lact); do
    if tr '\0' ' ' < /proc/$p/cmdline | grep -q 'lact daemon'; then
        echo "  killing leftover daemon pid $p"; kill "$p"; sleep 1
    fi
done

echo "== make install (PREFIX=/usr/local) =="
make install PREFIX=/usr/local

echo "== boot guard login notice (profile.d / fish) =="
install -m 644 res/boot-guard/lact-boot-guard.sh /etc/profile.d/lact-boot-guard.sh
if [ -d /etc/fish ]; then
    install -d /etc/fish/conf.d
    install -m 644 res/boot-guard/lact-boot-guard.fish /etc/fish/conf.d/lact-boot-guard.fish
fi
# zsh in a desktop terminal is neither a login shell nor reads profile.d;
# source the snippet from the system zshrc once.
install -d /etc/zsh
if ! grep -qs lact-boot-guard /etc/zsh/zshrc; then
    printf '\n# LACT boot guard notice\n[ -r /etc/profile.d/lact-boot-guard.sh ] && . /etc/profile.d/lact-boot-guard.sh\n' >> /etc/zsh/zshrc
fi

echo "== enabling service =="
systemctl daemon-reload
systemctl enable --now lactd
sleep 3
systemctl --no-pager --lines=0 status lactd | head -3
echo "== daemon log (RM lines) =="
journalctl -u lactd --no-pager --since "-1 min" | grep -iE 'RM clock|initialized|error' | cut -c1-160 | tail -5
echo
echo "installed: $(command -v lact)  ($(lact --help 2>/dev/null | head -1))"
echo "GUI: run 'lact gui' or use the LACT desktop entry."
