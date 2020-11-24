#!/bin/bash
set -euo pipefail

dn=$(cd $(dirname $0) && pwd)
. ${dn}/libtest.sh

image=$(pwd)/fcos.qcow2
if ! test -f "${image}" ; then
    fatal "${image} must exist"
fi
srcdir=$(cd ${dn} && git rev-parse --show-toplevel)

set -x
tmpdir=$(mktemp -d -p /tmp ccisp.XXXXXXX)
cd "${tmpdir}"
fcct ${srcdir}/run-qemu.fcc -o run.ign
qemuexec_args=(kola qemuexec --propagate-initramfs-failure --qemu-image "${image}" --qemu-firmware uefi \
    -i run.ign --bind-ro ${srcdir},/run/srcdir --bind-rw .,/run/testtmp)
disk_args=()
for n in 1 2; do
    path=$(pwd)/empty${n}.qcow2
    qemu-img create -f qcow2 ${path} 1G
    disk_args+=(-device nvme,drive=drive${n},serial=CoreOSQEMUInstance${n} -drive if=none,id=drive${n},file=${path})
done
runv ${qemuexec_args[@]} --devshell -- ${disk_args[@]}
rm "${tmpdir}" -rf