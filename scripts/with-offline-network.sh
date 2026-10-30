#!/bin/sh
# Validation-only network namespace: fixture loopback works, external routes do not.
# Toolchain/dependency/OS provisioning must finish before entering this boundary.
set -eu
# Do not create a user namespace: security tests deliberately inspect host UID
# maps. Privilege is used only to create the network namespace and bring up lo;
# setpriv drops back to the caller before running any validation code.
uid=$(id -u)
gid=$(id -g)
exec sudo -n --preserve-env=HOME,PATH,CARGO_HOME,RUSTUP_HOME,CARGO_TARGET_DIR,CARGO_NET_OFFLINE \
    unshare --net -- sh -eu -c '
        ip link set lo up
        uid=$1 gid=$2
        shift 2
        exec setpriv --reuid="$uid" --regid="$gid" --init-groups -- "$@"
    ' offline-network "$uid" "$gid" "$@"
