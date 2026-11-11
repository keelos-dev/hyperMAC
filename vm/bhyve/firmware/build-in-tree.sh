# b.sh [build arguments]: build ArmVirtQemuKernel (B=DEBUG|RELEASE, default DEBUG)
cd /root/edk2
export WORKSPACE=$PWD
args="$*"; set --
source ./edksetup.sh > /root/setup.log 2>&1
build -a AARCH64 -t GCC -b ${B:-DEBUG} -n 8 -p ArmVirtPkg/ArmVirtQemuKernel.dsc $args > /root/edk2-build-${B:-DEBUG}.log 2>&1 && echo BUILD-OK || { echo BUILD-FAILED; grep -n " error \|rror:" /root/edk2-build-${B:-DEBUG}.log | head; }
