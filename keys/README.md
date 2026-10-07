# Keys

Git-ignored. `devnet/` holds the program keypairs (`<program>-keypair.json`, the program IDs in
`Anchor.toml` and each `declare_id!`) and `deployer.json`, the devnet upgrade authority and admin.
Back them up off this machine: a lost program keypair means the program can never be upgraded,
and a lost deployer key means the configs can never be changed.

Mainnet keys do not live here. The deploy scripts take the paths of the mainnet program keypairs
and the authority as arguments.
