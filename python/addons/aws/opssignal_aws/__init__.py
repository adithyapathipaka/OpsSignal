"""AWS support for opssignal: configs from S3, AppConfig and DynamoDB,
Secrets Manager placeholders, and email through Amazon SES.

Installed by `pip install 'opssignal[aws]'`. Nothing to import or
configure: opssignal finds this package on its own and uses it when
SIGNAL_CONFIG is an s3://, appconfig:// or dynamodb:// URL, a
${aws-sm:...} placeholder, or an email provider with `ses:`.
"""

from opssignal_aws._aws import CLOUD, fetch, send

__all__ = ["CLOUD", "fetch", "send"]
