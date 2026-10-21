# opssignal-azure

Azure support for [opssignal](https://pypi.org/project/opssignal/): `az://` Blob Storage configs, Azure App Configuration (`azappconfig://`), Cosmos DB items (`cosmos://`) and `${azure-kv:...}` Key Vault placeholders.

Install it as an extra of opssignal rather than on its own:

```bash
pip install --pre 'opssignal[azure]'
```

opssignal picks the add-on up by itself; there is nothing to import. See
the [configuration docs](https://adithyapathipaka.github.io/OpsSignal/configuration/)
for the URL forms and credentials each service uses.

- Documentation: https://adithyapathipaka.github.io/OpsSignal/
- Source and issues: https://github.com/adithyapathipaka/OpsSignal
- License: Apache-2.0
