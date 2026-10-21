# opssignal-aws

AWS support for [opssignal](https://pypi.org/project/opssignal/): `s3://` configs, AWS AppConfig (`appconfig://`), DynamoDB items (`dynamodb://`), `${aws-sm:...}` Secrets Manager placeholders and email through the Amazon SES API.

Install it as an extra of opssignal rather than on its own:

```bash
pip install --pre 'opssignal[aws]'
```

opssignal picks the add-on up by itself; there is nothing to import. See
the [configuration docs](https://adithyapathipaka.github.io/OpsSignal/latest/configuration/)
for the URL forms and credentials each service uses.

- Documentation: https://adithyapathipaka.github.io/OpsSignal/
- Source and issues: https://github.com/adithyapathipaka/OpsSignal
- License: Apache-2.0
