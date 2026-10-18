"""AWS S3 and AppConfig support for opssignal.

Installed by `pip install 'opssignal[aws]'`. Nothing to import or
configure: opssignal finds this package on its own and uses it when
SIGNAL_CONFIG is an s3:// or appconfig:// URL.
"""

from opssignal_aws._aws import CLOUD, fetch

__all__ = ["CLOUD", "fetch"]
