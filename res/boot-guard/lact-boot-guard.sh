# LACT boot guard (fork): print the login notice in interactive shells.
# Installed to /etc/profile.d/ by install_fork.sh; pam_motd shows the same
# file on tty and SSH logins, this covers terminals opened from the desktop.
if [ -r /run/motd.d/lact-boot-guard ]; then
    case "$-" in *i*) cat /run/motd.d/lact-boot-guard ;; esac
fi
