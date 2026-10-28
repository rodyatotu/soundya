#!/bin/sh
# Builds packaging/shairport-sync: the AirPlay receiver soundya runs on the speaker.
# A static ARMv7 binary (musl), so it needs nothing from the speaker's firmware.
#
# Runs on x86_64 Linux with clang/lld 18, llvm-ar, autoconf, automake, libtool, git:
#   sudo apt install clang lld llvm autoconf automake libtool git make
#   sh scripts/build-shairport.sh
#
# Versions: shairport-sync 5.6.2 (classic AirPlay, stdout backend), musl 1.2.5,
# compiler-rt 18.1.8 builtins, mbedTLS 2.28.9, libconfig 1.8.1, popt 1.19.
set -eu
ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
W=${SOUNDYA_CACHE:-"${XDG_CACHE_HOME:-$HOME/.cache}/soundya"}/shairport
SYS=$W/sysroot
T=armv7-linux-musleabihf
ARCH="-march=armv7-a -mfpu=neon-vfpv4 -mfloat-abi=hard -mthumb"
CLANG_INC=$(clang -print-resource-dir)/include
AR=$(command -v llvm-ar-18 || command -v llvm-ar)
RANLIB=$(command -v llvm-ranlib-18 || command -v llvm-ranlib)
STRIP=$(command -v llvm-strip-18 || command -v llvm-strip)
mkdir -p "$W" "$SYS/lib" "$W/bin" "$W/res/lib/linux"
cd "$W"

get() { # repo tag dir
    [ -d "$3" ] || git clone -q --depth 1 -b "$2" "https://github.com/$1" "$3"
}
get kraj/musl v1.2.5 musl
get Mbed-TLS/mbedtls v2.28.9 mbedtls
get hyperrealm/libconfig v1.8.1 libconfig
get rpm-software-management/popt popt-1.19-release popt
get mikebrady/shairport-sync 5.6.2 shairport-sync
if [ ! -d llvm ]; then
    git clone -q --depth 1 --filter=blob:none --sparse -b llvmorg-18.1.8 https://github.com/llvm/llvm-project llvm
    git -C llvm sparse-checkout set compiler-rt/lib/builtins
fi

# Compiler wrapper: clang for the speaker, static, against our musl sysroot.
cat > bin/armcc <<EOF
#!/bin/sh
exec clang --target=$T $ARCH --sysroot=$SYS -nostdinc -isystem $SYS/include -isystem $CLANG_INC \
  -static -fuse-ld=lld -rtlib=compiler-rt --unwindlib=none -resource-dir=$W/res \
  -Wno-unused-command-line-argument "\$@"
EOF
chmod +x bin/armcc
PATH=$W/bin:$PATH
ln -sfn . "$SYS/usr"

# 1. musl headers, compiler-rt builtins, musl.
if [ ! -f "$SYS/lib/libc.a" ]; then
    (cd musl && ./configure --target=arm-linux-musleabihf --prefix="$SYS" --disable-shared \
        CC="clang --target=$T $ARCH" AR="$AR" RANLIB="$RANLIB" LIBCC=" " >/dev/null && make -s install-headers)
    B=llvm/compiler-rt/lib/builtins
    rm -rf rt && mkdir rt
    for f in $B/*.c $B/arm/aeabi_*.S $B/arm/aeabi_*.c $B/arm/*divmod*.S $B/arm/divsi3.S $B/arm/udivsi3.S \
             $B/arm/modsi3.S $B/arm/umodsi3.S $B/arm/clz*.S $B/arm/bswap*.S $B/arm/sync_synchronize.S; do
        [ -f "$f" ] || continue
        # x86 long double helpers do not build for ARM: skipping them is expected.
        clang --target=$T $ARCH -Os -nostdinc -isystem "$SYS/include" -isystem "$CLANG_INC" -ffreestanding \
            -fno-builtin -I$B -c "$f" -o "rt/$(echo "$f" | sed 's|.*/builtins/||; s|/|_|g').o" 2>/dev/null || true
    done
    "$AR" rcs "$SYS/lib/libclang_rt.builtins.a" rt/*.o
    cp "$SYS/lib/libclang_rt.builtins.a" res/lib/linux/libclang_rt.builtins-armhf.a
    (cd musl && make -s -j"$(nproc)" LIBCC="$SYS/lib/libclang_rt.builtins.a" >/dev/null \
        && make -s install LIBCC="$SYS/lib/libclang_rt.builtins.a" >/dev/null)
    # musl needs no crtbegin/crtend, but clang's driver asks for them.
    echo "" > empty.c
    for o in crtbeginT.o crtbegin.o crtend.o crtbeginS.o crtendS.o; do
        clang --target=$T -mfloat-abi=hard -c empty.c -o "$SYS/lib/$o"
    done
fi

# 2. Libraries.
if [ ! -f "$SYS/lib/libmbedtls.a" ]; then
    make -s -C mbedtls -j"$(nproc)" lib CC=armcc AR="$AR" CFLAGS="-Os -ffunction-sections -fdata-sections" >/dev/null
    cp mbedtls/library/*.a "$SYS/lib/"
    cp -r mbedtls/include/mbedtls mbedtls/include/psa "$SYS/include/"
fi
if [ ! -f "$SYS/lib/libconfig.a" ]; then
    (cd libconfig && autoreconf -fi >/dev/null 2>&1 &&
        ./configure -q --host=arm-linux-musleabihf --prefix="$SYS" --disable-shared --enable-static \
            --disable-cxx --disable-examples --disable-tests CC=armcc AR="$AR" RANLIB="$RANLIB" CFLAGS="-Os" &&
        make -s -C lib -j"$(nproc)" >/dev/null && make -s -C lib install >/dev/null)
fi
if [ ! -f "$SYS/lib/libpopt.a" ]; then
    mkdir -p popt/o
    D="-DHAVE_STPCPY -DHAVE_STRERROR -DHAVE_SECURE_GETENV -DHAVE_GLOB_H -DHAVE_GLOB -DHAVE_FNMATCH_H \
       -DHAVE_FNMATCH -DHAVE_UNISTD_H -DHAVE_STDINT_H -DHAVE_STRING_H -DHAVE_LIMITS_H -DHAVE_SRANDOM \
       -DHAVE_ALLOCA_H -DHAVE_MBSRTOWCS -DHAVE_LANGINFO_H -DHAVE_NL_LANGINFO -D_GNU_SOURCE \
       -DPOPT_SYSCONFDIR=\"/etc\" -DPACKAGE=\"popt\" -DVERSION=\"1.19\""
    for f in popt poptconfig popthelp poptint poptparse lookup3; do
        # shellcheck disable=SC2086
        armcc -Os $D -Ipopt/src -c popt/src/$f.c -o popt/o/$f.o
    done
    "$AR" rcs "$SYS/lib/libpopt.a" popt/o/*.o
    cp popt/src/popt.h "$SYS/include/"
    mkdir -p "$SYS/lib/pkgconfig"
    printf 'Name: popt\nDescription: popt\nVersion: 1.19\nLibs: -L%s/lib -lpopt\nCflags: -I%s/include\n' \
        "$SYS" "$SYS" > "$SYS/lib/pkgconfig/popt.pc"
fi

# 3. shairport-sync: classic AirPlay, sound to stdout (soundya plays it),
# announced through soundya's mDNS (external-avahi → soundya's stand-in).
cd shairport-sync
autoreconf -fi >/dev/null 2>&1
PKG_CONFIG_LIBDIR=$SYS/lib/pkgconfig ./configure -q --host=arm-linux-musleabihf --sysconfdir=/data/opt/soundya \
    --without-ffmpeg --with-hammerton --with-stdout --with-pipe --with-ssl=mbedtls --with-external-mdns \
    --with-metadata CC=armcc CXX=armcc AR="$AR" RANLIB="$RANLIB" \
    CFLAGS="-Os -ffunction-sections -fdata-sections" LDFLAGS="-Wl,--gc-sections -L$SYS/lib" 2>/dev/null
make -s clean >/dev/null 2>&1 || true
make -s -j"$(nproc)" CXXLD=armcc \
    LIBS="-lmbedtls -lmbedx509 -lmbedcrypto -lconfig -lpopt -lm -lpthread -lrt" >/dev/null 2>&1
"$STRIP" -o "$ROOT/packaging/shairport-sync" shairport-sync
chmod 755 "$ROOT/packaging/shairport-sync"
echo "Built packaging/shairport-sync ($(wc -c < "$ROOT/packaging/shairport-sync") bytes)"
