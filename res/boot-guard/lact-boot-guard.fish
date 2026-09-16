# LACT boot guard (fork): print the login notice in interactive fish shells.
# Installed to /etc/fish/conf.d/ by install_fork.sh.
if status is-interactive; and test -r /run/motd.d/lact-boot-guard
    cat /run/motd.d/lact-boot-guard
end
