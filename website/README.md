# Napstr website

This folder is a dependency-free static website styled like a circa-2000 music-sharing homepage.
The Home, Download, Napstrfy, Nostr, and Tor pages share the same static navigation and layout.

Preview it locally from the repository root:

```bash
npx serve website
```

The download page loads the latest published GitHub Release through GitHub's public API and
matches the Tauri-generated desktop installers. The Napstrfy page uses the same release lookup
to find an attached Android `.apk`. On a
standard `owner.github.io/repository` Pages URL, `releases.js` derives the repository name from
the page address automatically.

The `github-repository` meta value in `download.html` points to `lnbits/napstr`, so release
downloads also work when the Pages site uses a custom domain.

`.github/workflows/pages.yml` deploys this folder to GitHub Pages. In the repository settings,
select **GitHub Actions** as the Pages source. Tagged release builds are published by
`.github/workflows/release.yml`, making their assets visible to the download page.

The generated Napstr logo is stored at `website/assets/napstr-logo.png`.
