import copy
import datetime
import importlib.util
import json
import os
import shutil
import subprocess
from pathlib import Path
import plistlib
import tempfile
import unittest
import zipfile

spec = importlib.util.spec_from_file_location('verify_ios', Path(__file__).with_name('verify-ios.py'))
verify = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verify)


class SigningTests(unittest.TestCase):
    def setUp(self):
        self.profile = {
            'TeamIdentifier': ['TEAM'], 'ApplicationIdentifierPrefix': ['TEAM'],
            'ExpirationDate': datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(days=10),
            'Entitlements': {'application-identifier': 'TEAM.net.napstr.nostrfy', 'get-task-allow': False},
        }

    def test_correct_profile_and_wrong_app_team_expiry(self):
        verify.validate_profile(self.profile, 'net.napstr.nostrfy', 'TEAM', 'app-store-connect')
        for key, value in [('TeamIdentifier', ['OTHER']), ('ExpirationDate', datetime.datetime(2000, 1, 1)),
                           ('Entitlements', {'application-identifier': 'TEAM.com.nostr.anagram'})]:
            profile = copy.deepcopy(self.profile)
            profile[key] = value
            with self.assertRaises(ValueError):
                verify.validate_profile(profile, 'net.napstr.nostrfy', 'TEAM', 'app-store-connect')

    def test_export_method_matches_device_restrictions(self):
        self.profile['ProvisionedDevices'] = ['test-device']
        verify.validate_profile(self.profile, 'net.napstr.nostrfy', 'TEAM', 'release-testing')
        with self.assertRaises(ValueError):
            verify.validate_profile(self.profile, 'net.napstr.nostrfy', 'TEAM', 'app-store-connect')
        self.profile['Entitlements']['get-task-allow'] = True
        verify.validate_profile(self.profile, 'net.napstr.nostrfy', 'TEAM', 'debugging')
        with self.assertRaises(ValueError):
            verify.validate_profile(self.profile, 'net.napstr.nostrfy', 'TEAM', 'release-testing')

    def test_ipa_version_permissions_and_device_build(self):
        info = {'CFBundleIdentifier': 'net.napstr.nostrfy', 'CFBundleShortVersionString': '1.2.3',
                'CFBundleVersion': '17', 'CFBundleSupportedPlatforms': ['iPhoneOS'],
                'NSCameraUsageDescription': 'Scan pairing code', 'NSLocalNetworkUsageDescription': 'Connect to Napstr'}
        with tempfile.TemporaryDirectory() as directory:
            ipa = Path(directory) / 'app.ipa'
            def package(value):
                with zipfile.ZipFile(ipa, 'w') as archive:
                    archive.writestr('Payload/Napstrfy.app/Info.plist', plistlib.dumps(value))
                    archive.writestr('Payload/Napstrfy.app/PrivacyInfo.xcprivacy', plistlib.dumps({'NSPrivacyAccessedAPITypes': [{'NSPrivacyAccessedAPIType': 'NSPrivacyAccessedAPICategoryFileTimestamp'}]}))
            package(info)
            verify.verify_ipa(ipa, 'net.napstr.nostrfy', '1.2.3', '17', True)
            for key, value in [('CFBundleVersion', '16'), ('NSCameraUsageDescription', ''), ('CFBundleSupportedPlatforms', ['iPhoneSimulator'])]:
                package({**info, key: value})
                with self.assertRaises(ValueError):
                    verify.verify_ipa(ipa, 'net.napstr.nostrfy', '1.2.3', '17', True)
            package(info)
            with self.assertRaises(ValueError):
                verify.verify_ipa(ipa, 'net.napstr.nostrfy', '1.2.3', '17', False)


class PackagingModeTests(unittest.TestCase):
    def run_package(self, credentials):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'scripts').mkdir()
            (root / 'bin').mkdir()
            shutil.copyfile(Path(__file__).with_name('package-ios.sh'), root / 'scripts/package-ios.sh')
            (root / 'scripts/ios-release.mjs').write_text('// version helper stub\n')
            (root / 'package.json').write_text(json.dumps({'version': '1.2.3'}))
            (root / 'bin/npm').write_text('#!/bin/sh\nmkdir -p src-tauri/gen/apple/build\nprintf "fixture IPA" > src-tauri/gen/apple/build/app.ipa\nprintf "%s\\n" "$@" > build-arguments\n')
            (root / 'bin/python3').write_text('#!/bin/sh\nexit 0\n')
            for name in ['npm', 'python3']:
                (root / 'bin' / name).chmod(0o700)
            env = {key: value for key, value in os.environ.items()
                   if not key.startswith('IOS_') and key not in ('APPLE_DEVELOPMENT_TEAM', 'RELEASE_TAG', 'GITHUB_STEP_SUMMARY')}
            env.update(credentials)
            env['PATH'] = str(root / 'bin') + os.pathsep + env['PATH']
            result = subprocess.run(['bash', str(root / 'scripts/package-ios.sh')], env=env, capture_output=True, text=True)
            args = (root / 'build-arguments').read_text() if (root / 'build-arguments').exists() else ''
            artifacts = [path.name for path in (root / 'ios-installers').glob('*')]
            return result, args, artifacts

    def test_missing_signing_material_builds_clearly_named_unsigned_ipa(self):
        result, args, artifacts = self.run_package({'APPLE_DEVELOPMENT_TEAM': 'TEAM'})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('--no-sign', args)
        self.assertIn('Napstrfy_1.2.3_ios-unsigned.ipa', artifacts)
        self.assertIn('Napstrfy_1.2.3_ios-unsigned.ipa.sha256', artifacts)

    def test_partial_signing_configuration_fails_before_build(self):
        result, args, artifacts = self.run_package({'APPLE_DEVELOPMENT_TEAM': 'TEAM', 'IOS_CERTIFICATE': 'fake-certificate'})
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(args, '')
        self.assertEqual(artifacts, [])

    def test_complete_signing_configuration_uses_requested_export_method(self):
        result, args, artifacts = self.run_package({
            'APPLE_DEVELOPMENT_TEAM': 'TEAM', 'IOS_CERTIFICATE': 'fake-certificate',
            'IOS_CERTIFICATE_PASSWORD': 'fake-password', 'IOS_MOBILE_PROVISION': 'fake-profile',
            'IOS_EXPORT_METHOD': 'release-testing', 'RELEASE_TAG': 'v1.2.3-rc1',
        })
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn('--no-sign', args)
        self.assertIn('release-testing', args)
        self.assertIn('Napstrfy_1.2.3-rc1_ios.ipa', artifacts)
