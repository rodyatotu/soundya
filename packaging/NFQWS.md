# nfqws

`packaging/nfqws` is zapret's nfqws, unchanged, from the official release:

- release: https://github.com/bol-van/zapret/releases/tag/v72.9 (`zapret-v72.9.tar.gz`,
  sha256 `1e14dc6320dd7b5a1f28ab8145ac76d7a7010ca3cbe71824aa4aa407f7279ddf`)
- file: `zapret-v72.9/binaries/linux-arm/nfqws` (static ARMv7)
- license: MIT, © bol-van — https://github.com/bol-van/zapret

soundya starts it for «Обход блокировок» when its own tricks are too slow (src/zapret.rs).
To update: take the same file from a newer release and replace this one.

`packaging/fake/*.bin` are zapret's fake TLS ClientHello files from the same release
(`zapret-v72.9/files/fake/`), used by the strategies as `--dpi-desync-fake-tls=@…`.

The first strategy («Станция (alice-zapret2)») is the TLS strategy of
https://github.com/lr1ne/alice-zapret2 (zapret2 on Yandex Stations), written in nfqws v1 terms.
