"""AWS S3 support for opssignal.

Installed by `pip install 'opssignal[aws]'`. Nothing to import or
configure: opssignal finds this package on its own and uses it when
SIGNAL_CONFIG is a s3:// URL.
"""

from opssignal_aws._aws import SCHEME, fetch

__all__ = ["SCHEME", "fetch"]
