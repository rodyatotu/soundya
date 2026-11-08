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

Two more, for «general (ALT11)», are from https://github.com/Flowseal/zapret-discord-youtube
(`bin/`, commit `4e71116`, 2026-07; MIT), unchanged — data the strategy sends as decoys, not programs:

- `stun2.bin` (120 bytes, a STUN binding request), sha256 `b7c2497496039c541f7337ac8536813f0a1cf52363ab2faa5213b7816d458813`
- `tls_clienthello_www_sferum_ru.bin` (661 bytes, a TLS ClientHello for www.sferum.ru),
  sha256 `e6dd8d724eecc6a7340bf64e2ebec81c60bc4e078c6f702b4b6ba9bf184180ad`

The first strategy («Станция (alice-zapret2)») is the TLS strategy of
https://github.com/lr1ne/alice-zapret2 (zapret2 on Yandex Stations), written in nfqws v1 terms.

# nfqws2 (zapret2)

`packaging/nfqws2` is zapret2's nfqws2, unchanged, from the official release:

- release: https://github.com/bol-van/zapret2/releases/tag/v1.0.6 (`zapret2-v1.0.6.tar.gz`,
  sha256 `eb412aebc82cdef1f09b6aa9df45c8ff60fb88683b6663e6c4a9275d87ea2db4`)
- file: `zapret2-v1.0.6/binaries/linux-arm/nfqws2` (static ARMv7,
  sha256 `32dc4a573830a334d4cec48194814e9eb6a1ad312cb4f630beefcdc157128c8b`)
- `packaging/lua/zapret-lib.lua`, `zapret-antidpi.lua`, `zapret-auto.lua`: `zapret2-v1.0.6/lua/`, unchanged
- license: MIT, © bol-van — https://github.com/bol-van/zapret2

The «zapret2: …» strategies in src/zapret.rs are its config.default for TLS and
strategies from its blockcheck2.d/standard. To update: take the same files from a
newer release.
