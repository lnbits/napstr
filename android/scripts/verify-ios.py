"""Check iOS packaging without printing certificates, profiles or device IDs."""
import base64
import datetime
import json
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile
import zipfile


def validate_profile(profile, identifier, team, method):
    if method not in ('app-store-connect', 'release-testing', 'debugging'):
        raise ValueError('IOS_EXPORT_METHOD must be app-store-connect, release-testing or debugging')
    if team not in profile.get('TeamIdentifier', []):
        raise ValueError('iOS provisioning profile does not match APPLE_TEAM_ID')
    expiry = profile.get('ExpirationDate')
    if not isinstance(expiry, datetime.datetime) or expiry.replace(tzinfo=datetime.timezone.utc) <= datetime.datetime.now(datetime.timezone.utc):
        raise ValueError('iOS provisioning profile is expired or has no expiry')
    entitlements = profile.get('Entitlements', {})
    app_id = entitlements.get('application-identifier', '')
    prefixes = profile.get('ApplicationIdentifierPrefix', [team])
    matches = any(app_id == f'{prefix}.{identifier}' or
                  (app_id.startswith(f'{prefix}.') and app_id.endswith('.*') and
                   f'{prefix}.{identifier}'.startswith(app_id[:-1])) for prefix in prefixes)
    if not matches:
        raise ValueError('iOS provisioning profile must cover the Napstrfy bundle identifier')
    development = entitlements.get('get-task-allow', False)
    devices = bool(profile.get('ProvisionedDevices'))
    enterprise = bool(profile.get('ProvisionsAllDevices'))
    if method == 'app-store-connect' and (development or devices or enterprise):
        raise ValueError('App Store export requires an App Store provisioning profile')
    if method == 'release-testing' and (development or not devices or enterprise):
        raise ValueError('Release-testing export requires an ad hoc profile with registered devices')
    if method == 'debugging' and (not development or not devices):
        raise ValueError('Debugging export requires a development profile with registered devices')


def decode_profile(data):
    with tempfile.TemporaryDirectory(prefix='napstrfy-ios-profile-') as directory:
        path = Path(directory) / 'profile.mobileprovision'
        path.write_bytes(data)
        path.chmod(0o600)
        result = subprocess.run(['security', 'cms', '-D', '-i', str(path)], capture_output=True)
        if result.returncode:
            raise ValueError('Could not decode iOS provisioning profile')
        return plistlib.loads(result.stdout)


def verify_ipa(path, identifier, version, build, unsigned):
    with zipfile.ZipFile(path) as archive:
        roots = [name for name in archive.namelist()
                 if name.startswith('Payload/') and name.endswith('.app/Info.plist') and name.count('/') == 2]
        if len(roots) != 1:
            raise ValueError('Expected one iPhone app inside the IPA')
        root = roots[0][:-len('Info.plist')]
        info = plistlib.loads(archive.read(roots[0]))
        for key, expected in [('CFBundleIdentifier', identifier), ('CFBundleShortVersionString', version), ('CFBundleVersion', build)]:
            if info.get(key) != expected:
                raise ValueError(f'IPA {key} does not match the requested build')
        if not info.get('NSCameraUsageDescription') or not info.get('NSLocalNetworkUsageDescription'):
            raise ValueError('IPA is missing camera or local-network permission descriptions')
        if info.get('CFBundleSupportedPlatforms') != ['iPhoneOS']:
            raise ValueError('IPA must contain a device build, not a simulator app')
        privacy = plistlib.loads(archive.read(root + 'PrivacyInfo.xcprivacy'))
        if not privacy.get('NSPrivacyAccessedAPITypes'):
            raise ValueError('IPA is missing its required-reason privacy manifest')
        if not unsigned:
            if root + '_CodeSignature/CodeResources' not in archive.namelist():
                raise ValueError('Signed IPA is missing its code signature')
            validate_profile(decode_profile(archive.read(root + 'embedded.mobileprovision')),
                             identifier, os.environ['APPLE_DEVELOPMENT_TEAM'], os.environ['IOS_EXPORT_METHOD'])
            with tempfile.TemporaryDirectory(prefix='napstrfy-ios-verify-') as directory:
                unpack = subprocess.run(['ditto', '-x', '-k', str(path), directory], capture_output=True)
                if unpack.returncode:
                    raise ValueError('Could not unpack IPA for signature verification')
                app = Path(directory) / root
                signed = subprocess.run(['codesign', '--verify', '--deep', '--strict', str(app)], capture_output=True)
                if signed.returncode:
                    raise ValueError('IPA code signature verification failed')


def main():
    config = json.loads(Path('src-tauri/tauri.conf.json').read_text())
    if sys.argv[1] == 'profile':
        encoded = ''.join(os.environ['IOS_MOBILE_PROVISION'].split())
        validate_profile(decode_profile(base64.b64decode(encoded, validate=True)),
                         config['identifier'], os.environ['APPLE_DEVELOPMENT_TEAM'], os.environ['IOS_EXPORT_METHOD'])
    elif sys.argv[1] == 'ipa':
        version = json.loads(Path('src-tauri/tauri.ios.ci.conf.json').read_text())
        verify_ipa(sys.argv[2], config['identifier'], version['version'],
                   version['bundle']['iOS']['bundleVersion'], sys.argv[3] == '-unsigned')
    else:
        raise ValueError('Expected profile or ipa')
    print('iOS packaging validation passed')


if __name__ == '__main__':
    try:
        main()
    except (ValueError, KeyError, OSError, zipfile.BadZipFile) as error:
        # Only our validation messages are suitable for CI; plist/base64 failures
        # may otherwise include parts of the supplied credentials.
        print(f'iOS validation failed: {error}' if type(error) is ValueError else 'iOS validation failed: invalid or missing package data', file=sys.stderr)
        sys.exit(1)
