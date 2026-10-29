# Security

## Reporting a vulnerability

Please report security issues privately through GitHub's
[private vulnerability reporting](https://github.com/zackarychapple/home-runner/security/advisories/new)
rather than in a public issue. You should get a response within a week.

## Using self-hosted runners safely

home-runner executes whatever code a workflow tells it to, on your hardware.

- **Never attach these runners to public repositories.** Anyone who can open a pull request could run
  code on your machines. Keep "Allow public repositories" off in the runner group.
- `host` pools (Windows, Intel macOS) run jobs directly on the machine, so one job can leave things
  behind for the next. VM-backed pools (`apple-container`, `tart`) start every job from a clean image.
- Put runner machines on their own network segment and block them from reaching the rest of your LAN.
- Prefer a GitHub App over a personal access token, and keep its private key readable only by the
  account home-runner runs as.
