#!/bin/sh
# In the build VM: fetch EDK2 (tag given) and build a platform for AARCH64.
tag=$1; shift
cd /root
[ -d edk2 ] || git clone -q --depth 1 -b "$tag" https://github.com/tianocore/edk2 || exit 1
cd edk2
for m in BaseTools/Source/C/BrotliCompress/brotli MdeModulePkg/Library/BrotliCustomDecompressLib/brotli \
    MdePkg/Library/BaseFdtLib/libfdt MdePkg/Library/MipiSysTLib/mipisyst \
    CryptoPkg/Library/OpensslLib/openssl CryptoPkg/Library/MbedTlsLib/mbedtls \
    SecurityPkg/DeviceSecurity/SpdmLib/libspdm MdeModulePkg/Universal/RegularExpressionDxe/oniguruma \
    RedfishPkg/Library/JsonLib/jansson; do
	[ -n "$(ls -A $m 2>/dev/null)" ] || git submodule update -q --init --depth 1 "$m" || echo "submodule $m failed"
done
export WORKSPACE=$PWD
args="$*"
set --		# edksetup.sh takes the positional parameters for its own
. ./edksetup.sh >/dev/null
make -s -C BaseTools -j8 >/root/basetools.log 2>&1 || { echo BASETOOLS-FAILED; tail -5 /root/basetools.log; exit 1; }
build -a AARCH64 -t GCC -b RELEASE -n 8 $args > /root/edk2-build.log 2>&1 && echo BUILD-OK || { echo BUILD-FAILED; grep -n "error\|Error" /root/edk2-build.log | head -20; }
ls -la Build/*/RELEASE_GCC*/FV/*.fd 2>/dev/null
