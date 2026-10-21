# opssignal-gcp

Google Cloud support for [opssignal](https://pypi.org/project/opssignal/): `gs://` configs, Firestore documents (`firestore://`) and `${gcp-sm:...}` Secret Manager placeholders.

Install it as an extra of opssignal rather than on its own:

```bash
pip install --pre 'opssignal[gcp]'
```

opssignal picks the add-on up by itself; there is nothing to import. See
the [configuration docs](https://adithyapathipaka.github.io/OpsSignal/configuration/)
for the URL forms and credentials each service uses.

- Documentation: https://adithyapathipaka.github.io/OpsSignal/
- Source and issues: https://github.com/adithyapathipaka/OpsSignal
- License: Apache-2.0
