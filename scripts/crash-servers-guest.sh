#!/bin/sh
# Eclipse OS: kill and freeze every userspace server of a running system
# while a client uses it, and say what the client saw. Run by
# scripts/qemu-crash-servers.py over the console; it can be run by hand on
# any Eclipse whose init state file lists the services below.
#
#   crash-servers-guest.sh [-p PORT] [CHECK...]
#
# PORT is a web server's on the host (10.0.2.2) with a file /blob, for the
# network checks; without it they are skipped. CHECK names a check or a
# family (disk, fs, net, input, audio); all of them without one.
#
# Each check prints one line:
#   PASS name                 the client saw nothing
#   FAIL name: why            it did
#   XFAIL name: why           it did, and that is a known gap (the plan
#                             in the thread "Reiniciar servidores caídos
#                             automáticamente" closes each one)
#   XPASS name                a known gap that is closed: move it out of
#                             KNOWN below
#   SKIP name: why            nothing to check on this machine
# and the last line is `crash-servers: N failed`. Exit status 1 when a check
# failed or a known gap passed, 0 otherwise.

S=/run/eclipse-init/services
PORT=
if [ "$1" = -p ]; then PORT=$2; shift 2; fi
WANT=" $* "

# The known gaps: checks expected to fail until their step is done.
# input.frozen and audio.frozen: a client that waits for an input event or
# for room in the sound buffer waits on the kernel, with no request out to
# the driver, so a frozen driver is never found out (one with a request out
# is killed after 10 s).
KNOWN=" fs.fat.watcher input.death input.frozen audio.death audio.frozen "

failed=0
report() { # report name ok|why
    name=$1; shift
    known=no; case $KNOWN in *" $name "*) known=yes ;; esac
    if [ "$1" = ok ]; then
        if [ $known = yes ]; then echo "XPASS $name"; failed=$((failed+1)); else echo "PASS $name"; fi
    else
        if [ $known = yes ]; then echo "XFAIL $name: $*"; else echo "FAIL $name: $*"; failed=$((failed+1)); fi
    fi
}
wanted() { # wanted name: whether this check is asked for
    [ "$WANT" = "  " ] && return 0
    for w in $WANT; do
        case $1 in "$w"|"$w".*) return 0 ;; esac
    done
    return 1
}

field() { awk -F'\t' -v n="$1" -v f="$2" '$1==n{print $f}' $S; }
pid_of() { field "$1" 4; }
running() { [ "$(field "$1" 3)" = running ]; }
now() { cut -d. -f1 /proc/uptime; }

# back NAME OLDPID [SECONDS]: wait for init to run NAME again under a new
# pid; prints the pid.
back() {
    i=0; n=$(( ${3:-30} * 2 ))
    while [ $i -lt $n ]; do
        p=$(pid_of "$1")
        if [ "$p" != "$2" ] && [ "$p" != - ] && kill -0 "$p" 2>/dev/null; then echo "$p"; return 0; fi
        sleep 0.5; i=$((i+1))
    done
    return 1
}

# done_within PID SECONDS: wait for a background client to end; 1 if it
# is still there (it is left: a client stuck in the kernel cannot be killed).
done_within() {
    i=0; n=$(( $2 * 2 ))
    while [ $i -lt $n ]; do
        kill -0 "$1" 2>/dev/null || return 0
        sleep 0.5; i=$((i+1))
    done
    return 1
}

# The disk each driver service serves, and its first partition: the scratch
# disks scripts/qemu-crash-servers.py gives the machine, a vfat with
# pattern.bin on each (tools/zx-virtio-blk/mkfsdisk.sh).
part_of() {
    case $1 in
        ublk) set -- /dev/vd?1 ;;
        unvme) set -- /dev/nvme?n?p1 ;;
        uahci) set -- /dev/sd?1 ;;
    esac
    for d in "$@"; do
        [ -b "$d" ] || continue
        mkdir -p /mnt/crash.probe
        if mount -t vfat "$d" /mnt/crash.probe 2>/dev/null; then
            if [ -f /mnt/crash.probe/pattern.bin ]; then umount /mnt/crash.probe; echo "$d"; return 0; fi
            umount /mnt/crash.probe
        fi
    done
    return 1
}

disk_checks() {
    svc=$1
    running "$svc" || { wanted "disk.$svc" && echo "SKIP disk.$svc: $svc is not running"; return; }
    part=$(part_of "$svc") || { echo "SKIP disk.$svc: no scratch vfat on its disk"; return; }
    m=/mnt/crash.$svc; mkdir -p $m
    mount -t vfat "$part" $m || { report "disk.$svc.death" "cannot mount $part"; return; }
    want=$(md5sum < $m/pattern.bin)

    if wanted "disk.$svc.death"; then
        # Copies of a file and a read of the whole disk, with the driver
        # killed under them.
        ( i=0; while [ $i -lt 10 ]; do cp $m/pattern.bin /tmp/crash.copy; md5sum < /tmp/crash.copy; i=$((i+1)); done > /tmp/crash.sums 2>&1 ) &
        copies=$!
        whole=${part%1}; whole=${whole%p}
        ( dd if="$whole" of=/dev/null bs=65536 count=256 2>/tmp/crash.dd; echo "rc=$?" >> /tmp/crash.dd ) &
        reader=$!
        sleep 0.3
        p=$(pid_of "$svc"); kill -9 "$p"
        new=$(back "$svc" "$p") || new=
        if ! done_within $copies 60 || ! done_within $reader 60; then
            report "disk.$svc.death" "a client is still waiting a minute after the driver died"
        elif [ -z "$new" ]; then
            report "disk.$svc.death" "init did not start $svc again"
        elif grep -q -v "$want" /tmp/crash.sums || [ "$(wc -l < /tmp/crash.sums)" != 10 ]; then
            report "disk.$svc.death" "a copy came out wrong: $(sort /tmp/crash.sums | uniq -c | tr '\n' ' ')"
        elif ! grep -q rc=0 /tmp/crash.dd; then
            report "disk.$svc.death" "the read of the disk failed: $(tr '\n' ' ' < /tmp/crash.dd)"
        else
            report "disk.$svc.death" ok
        fi
    fi

    if wanted "disk.$svc.frozen"; then
        # Remounted, so that the read goes to the disk.
        umount $m; mount -t vfat "$part" $m
        p=$(pid_of "$svc"); kill -STOP "$p"; t0=$(now)
        ( md5sum < $m/pattern.bin > /tmp/crash.frozen 2>&1 ) &
        client=$!
        if ! done_within $client 60; then
            report "disk.$svc.frozen" "the read is still waiting a minute after the driver froze"
        elif ! grep -q "$want" /tmp/crash.frozen; then
            report "disk.$svc.frozen" "the read came out wrong: $(cat /tmp/crash.frozen)"
        elif [ "$(pid_of "$svc")" = "$p" ]; then
            report "disk.$svc.frozen" "the frozen driver is still the disk's"
        else
            echo "     | read through a frozen $svc in $(( $(now) - t0 )) s"
            report "disk.$svc.frozen" ok
        fi
        kill -CONT "$p" 2>/dev/null
        back "$svc" "$p" > /dev/null
    fi
    umount $m 2>/dev/null
}

fat_server_pids() { # watcher first, then its server
    ps -o pid,args | grep "[e]clipse-fatfs .* $1\$" | awk '{print $1}'
}

fs_checks() {
    part=$(part_of ublk) || part=$(part_of unvme) || part=$(part_of uahci) || {
        echo "SKIP fs: no scratch vfat"; return; }
    m=/mnt/crash.fs; mkdir -p $m
    mount -t vfat "$part" $m || { report fs.fat.death "cannot mount $part"; return; }
    want=$(md5sum < $m/pattern.bin)
    set -- $(fat_server_pids $m); watcher=$1; server=$2
    if [ -z "$server" ]; then echo "SKIP fs: $m has no eclipse-fatfs server"; umount $m; return; fi

    if wanted fs.fat.death; then
        kill -9 "$server"; sleep 1
        if md5sum < $m/pattern.bin | grep -q "$want" && echo x > $m/crash.txt; then
            report fs.fat.death ok
        else
            report fs.fat.death "the mount failed after its server died"
        fi
    fi

    if wanted fs.fat.frozen; then
        set -- $(fat_server_pids $m); server=$2
        kill -STOP "$server"
        ( echo y > $m/crash2.txt && md5sum < $m/pattern.bin > /tmp/crash.fs 2>&1 ) &
        client=$!
        if ! done_within $client 60; then
            report fs.fat.frozen "the client is still waiting a minute after the server froze"
        elif ! grep -q "$want" /tmp/crash.fs; then
            report fs.fat.frozen "the read failed: $(cat /tmp/crash.fs)"
        else
            report fs.fat.frozen ok
        fi
        kill -CONT "$server" 2>/dev/null
    fi

    if wanted fs.fat.watcher; then
        set -- $(fat_server_pids $m); watcher=$1; server=$2
        kill -9 "$watcher"; sleep 1; kill -9 "$server"
        # Past the kernel's wait for the next server.
        sleep 12
        if echo z > $m/crash3.txt 2>/dev/null && [ "$(cat $m/crash3.txt 2>/dev/null)" = z ]; then
            report fs.fat.watcher ok
        else
            report fs.fat.watcher "with its watcher dead, nothing serves $m after its server dies"
        fi
    fi
    umount $m 2>/dev/null || umount -l $m 2>/dev/null
}

net_svc() { # the service of the network stack
    for s in enet unet knet; do running $s && { echo $s; return 0; }; done
    return 1
}

net_checks() {
    [ -n "$PORT" ] || { echo "SKIP net: no web server on the host (-p PORT)"; return; }
    svc=$(net_svc) || { echo "SKIP net: no network stack service runs"; return; }
    url=http://10.0.2.2:$PORT/blob
    wget -q -T 20 -O /dev/null $url || { echo "SKIP net: $url cannot be fetched before any check"; return; }

    if wanted net.death; then
        ( wget -q -T 60 -O /tmp/crash.blob $url; echo "rc=$?" > /tmp/crash.wget ) &
        client=$!
        sleep 2
        p=$(pid_of $svc); kill -9 "$p"
        new=$(back $svc "$p") || new=
        if ! done_within $client 120; then
            report net.death "the download is still going two minutes after the stack died"
        elif ! grep -q rc=0 /tmp/crash.wget; then
            report net.death "the download failed: $(cat /tmp/crash.wget)"
        elif [ -z "$new" ]; then
            report net.death "init did not start $svc again"
        else
            sleep 3
            if wget -q -T 30 -O /dev/null $url; then report net.death ok
            else report net.death "a new download failed after the restart"; fi
        fi
        rm -f /tmp/crash.blob
    fi

    if wanted net.frozen; then
        p=$(pid_of $svc); kill -STOP "$p"
        sleep 30
        if wget -q -T 30 -O /dev/null $url; then
            report net.frozen ok
        else
            report net.frozen "30 s after the stack froze the network is still down"
        fi
        kill -CONT "$p" 2>/dev/null
        sleep 3
    fi
}

# chardev NAME FAMILY SVC NODE CLIENT: the death and the freeze of a driver
# whose node a client holds open.
chardev_checks() {
    fam=$1; svc=$2; node=$3; shift 3
    running "$svc" || { echo "SKIP $fam.$svc: $svc is not running"; return; }
    [ -c "$node" ] || { echo "SKIP $fam.$svc: no $node"; return; }

    if wanted "$fam.death"; then
        ( "$@" > /dev/null 2>/tmp/crash.chr; echo "rc=$?" >> /tmp/crash.chr ) &
        client=$!
        sleep 1
        p=$(pid_of "$svc"); kill -9 "$p"
        back "$svc" "$p" > /dev/null
        sleep 3
        if [ "$fam" = input ]; then
            # A reader of an input node never ends by itself.
            if kill -0 $client 2>/dev/null; then report "$fam.death" ok
            else report "$fam.death" "its reader ended: $(tr '\n' ' ' < /tmp/crash.chr)"; fi
            kill $client 2>/dev/null
        else
            done_within $client 30
            if grep -q rc=0 /tmp/crash.chr; then report "$fam.death" ok
            else report "$fam.death" "its writer failed: $(tr '\n' ' ' < /tmp/crash.chr)"; fi
        fi
    fi

    if wanted "$fam.frozen"; then
        ( "$@" > /dev/null 2>&1 ) &
        client=$!
        sleep 1
        p=$(pid_of "$svc"); kill -STOP "$p"
        if new=$(back "$svc" "$p" 40); then report "$fam.frozen" ok
        else report "$fam.frozen" "the frozen $svc was not replaced in 40 s"; fi
        kill -CONT "$p" 2>/dev/null
        kill $client 2>/dev/null
    fi
}

# The node of a driver service: the one whose inode changes when the
# service starts again. Found by the device's name in sysfs where it has
# one; here by the order the boot serves them (virtio-input@0 is the
# keyboard QEMU adds first).
input_node() {
    for e in /sys/class/input/event*; do
        n=${e##*/}
        case "$(cat $e/device/name 2>/dev/null)" in *[Vv]irt*[Kk]ey*) echo /dev/input/$n; return 0 ;; esac
    done
    echo /dev/input/event1
}

[ -r $S ] || { echo "crash-servers: no $S: is this Eclipse with eclipse-init?"; exit 2; }

for svc in ublk unvme uahci; do
    wanted "disk.$svc" && disk_checks $svc
done
wanted fs && fs_checks
wanted net && net_checks
wanted input && chardev_checks input uinput "$(input_node)" cat "$(input_node)"
wanted audio && chardev_checks audio uhda /dev/dsp dd if=/dev/zero of=/dev/dsp bs=4096 count=200

echo "crash-servers: $failed failed"
[ $failed -eq 0 ]
