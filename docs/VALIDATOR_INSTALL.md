# Validator installation

This is the production installation path for a Monad validator host. The
sidecar runs as the dedicated, unprivileged `magma-sidecar` user and receives
ACL-only access to the mempool IPC socket. Do not add it to the `monad` group.

## 1. Install the package

Add the Magma APT repository (one-time), then install:

```bash
BASE="https://magmastaking.github.io/magma-sidecar-apt-repo"
sudo mkdir -p /etc/apt/keyrings
sudo wget -qO /etc/apt/keyrings/magma.gpg \
  "${BASE}/magma-apt-key.gpg.bin"
echo "deb [signed-by=/etc/apt/keyrings/magma.gpg] ${BASE} stable main" \
  | sudo tee /etc/apt/sources.list.d/magma.list
sudo apt update
sudo apt install magma-sidecar
```

APT verifies the signed repository metadata and package SHA256 automatically.
For independent key-fingerprint, signed-manifest, package-hash, and build
provenance checks, see
[`RELEASE_VERIFICATION.md`](RELEASE_VERIFICATION.md).

The package:

- creates the `magma-sidecar` system user through `systemd-sysusers`;
- installs a hardened native systemd unit;
- installs `/usr/lib/magma-sidecar/monad-ipc-setup`; and
- defaults `MAGMA_TXPOOL_SOCKET` to
`/var/run/monad-ipc/mempool.sock`.

The service is not enabled automatically. The node and RPC services must use
the same new socket path first.

## 2. Move the node socket to `/var/run/monad-ipc`

The stock Monad units currently use
`/home/monad/monad-bft/mempool.sock`. Configure both producers/consumers:

- `monad-bft.service`: create the runtime directory and ACLs before startup,
keep `UMask=0007`, and change `--mempool-ipc-path`.
- `monad-rpc.service`: change `--ipc-path`.

Systemd cannot replace one argument inside `ExecStart`; a drop-in must first
clear `ExecStart` (an empty `ExecStart=`) and then repeat the **complete**
command. Because that command differs by host and by Monad version, you must
build each drop-in from *your own* installed unit — not from a copied example.

> **Do not copy the commands in this section verbatim.** The `ExecStart` lines
> below are illustrative skeletons, not a working command. Copying them will
> overwrite your node's real arguments (identities, paths, CPU pinning,
> keystore, network) and can corrupt or break your validator. Build the drop-in
> from the output of `systemctl cat` as described in each step.



### `monad-bft.service` drop-in

**Step 1 — Read your current command.**

```bash
systemctl cat monad-bft
```

Locate the `ExecStart=` line (it starts at `/usr/local/bin/monad-node` and runs
to the last argument). This is *your* baseline. Keep this output visible.

**Step 2 — Open a drop-in.**

```bash
sudo systemctl edit monad-bft
```

**Step 3 — Add the three fixed lines.** These are identical on every host:

```ini
[Service]
UMask=0007
ExecStartPre=+/usr/lib/magma-sidecar/monad-ipc-setup
ExecStart=
```

**Step 4 — Append your own** `ExecStart`**, changing exactly one flag.** Paste the
`ExecStart` you read in Step 1 immediately below the empty `ExecStart=` line,
then change only the mempool socket path:

```diff
-    --mempool-ipc-path /home/monad/monad-bft/mempool.sock
+    --mempool-ipc-path /var/run/monad-ipc/mempool.sock
```

Leave every other argument exactly as your host had it (`--keystore-password ${KEYSTORE_PASSWORD}`, all paths, `--statesync-sq-thread-cpu`, `--otel-endpoint`,
etc.). Do not add or remove flags.

The result should have this shape — the `...` stands for *your* unchanged
arguments, which you must not retype from memory:

```ini
[Service]
UMask=0007
ExecStartPre=+/usr/lib/magma-sidecar/monad-ipc-setup
ExecStart=
ExecStart=/usr/local/bin/monad-node \
    ...              # every argument from `systemctl cat monad-bft`, verbatim
    --mempool-ipc-path /var/run/monad-ipc/mempool.sock \
    ...              # (the only changed value is the mempool path above)
    --keystore-password ${KEYSTORE_PASSWORD}
```

The `+` executable prefix is required: `User=monad` also applies to
`ExecStartPre` by default, while creating a directory under `/run` requires
root. The prefix runs only this setup command with full privileges before the
node starts as `monad`. On every start it recreates the tmpfs directory and
grants:

- `magma-sidecar:r-x` on the directory; and
- default `magma-sidecar:rw-` ACLs inherited by the new socket.



### `monad-rpc.service` drop-in

Repeat the same procedure for RPC, which reads the same socket:

**Step 1 — Read your current command.**

```bash
systemctl cat monad-rpc
```

**Step 2 — Open a drop-in** with `sudo systemctl edit monad-rpc`, add the reset
line, then paste your own `ExecStart` and change only `--ipc-path`:

```diff
-    --ipc-path /home/monad/monad-bft/mempool.sock
+    --ipc-path /var/run/monad-ipc/mempool.sock
```

Resulting shape (again, `...` is *your* unchanged arguments):

```ini
[Service]
ExecStart=
ExecStart=/usr/local/bin/monad-rpc \
    ...              # every argument from `systemctl cat monad-rpc`, verbatim
    --ipc-path /var/run/monad-ipc/mempool.sock \
    ...
```



### Sanity-check before starting

Confirm the merged unit has a single `monad-node` command with only the mempool
path changed, and that RPC points at the same socket:

```bash
systemctl cat monad-bft | grep -A100 '^\[Service\]'
systemd-analyze verify monad-bft.service
systemd-analyze verify monad-rpc.service
```

A doubled `ExecStart` (missing the empty reset line) or a mismatched socket path
between the two units is the most common mistake — fix it before proceeding.

## 3. Configure and start in order

```bash
sudo systemctl stop magma-sidecar monad-rpc monad-bft
sudo systemctl daemon-reload
sudo systemctl start monad-bft
sudo systemctl start monad-rpc

sudo nano /etc/magma-sidecar/sidecar.env
# Confirm:
# MAGMA_TXPOOL_SOCKET=/var/run/monad-ipc/mempool.sock
# MAGMA_NETWORK=mainnet

sudo systemctl enable --now magma-sidecar
```

## 4. Verify the sidecar is connected

Follow the service logs and confirm it loads its policy and connects to the
node's txpool over the ACL-protected socket:

```bash
journalctl -u magma-sidecar -f
```

A healthy startup shows the tip policy loading, then a successful IPC
connection — for example:

```
INFO magma_sidecar: loaded tip policy network=mainnet gateway=0x… base_fee_floor_wei=0
INFO magma_sidecar: connected to Monad txpool IPC path=/var/run/monad-ipc/mempool.sock
```

If instead you see the connection retrying, e.g.:

```
ERROR magma_sidecar: txpool IPC connect failed; retrying path=/var/run/monad-ipc/mempool.sock
```

then the node/RPC drop-ins or the ACL setup from step 2 are not yet in effect —
recheck those before continuing.

Do not use `chmod 666`, add `magma-sidecar` to the `monad` group, or expose
port 8089 on a validator network interface.