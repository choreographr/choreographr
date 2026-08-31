# choreographr.spec — RPM spec for the "fat" package: the four prebuilt
# binaries plus the systemd user unit.
#
# Staging assumption (why): nothing is compiled here. The binaries are prebuilt
# dist-profile artifacts (target/dist/, from the release pipeline or
# `cargo build --profile dist --workspace`)
# and scripts/build-rpm.sh stages them — together with
# packaging/choreographr.service — into a staging root, then invokes:
#     rpmbuild -bb --buildroot <staging> --define __os_install_post <nil> \
#       packaging/rpm/choreographr.spec
# (the __os_install_post define disables rpm's brp post-install processing.
# Our binaries are already stripped by the workspace [profile.dist] (inherits
# [profile.release]) strip = "symbols" — see the root Cargo.toml — so brp-strip
# would be a no-op anyway;
# we disable it so brp never re-processes or rewrites the prebuilt artifacts.)
# %files below therefore lists the staged layout verbatim; there are no
# BuildRequires, no Source tarball, and no %prep/%build/%install sections.
#
# No %post/%preun: the daemon is a *user* service. Packaging policy is
# "installed, never auto-enabled" — enabling a user unit from a %post script
# would require running systemctl as the invoking user, which package managers
# do not (and should not) do. The user opts in with:
#     systemctl --user enable --now choreographr

Name:           choreographr
Version:        0.1.0
Release:        1%{?dist}
Summary:        Agentic coding assistant — daemon, TUI, and bridges
License:        Apache-2.0
URL:            https://choreographr.com
BuildArch:      x86_64

%description
Choreographr is an agentic coding assistant: a local daemon (choreographr)
with a terminal UI (choreo-tui), an instant-messaging bridge (choreo-im),
and an ACP bridge (choreo-acp). This package ships the prebuilt
0.1.0 binaries and the systemd user unit (installed, never auto-enabled).

%files
/usr/bin/choreographr
/usr/bin/choreo-tui
/usr/bin/choreo-im
/usr/bin/choreo-acp
/usr/lib/systemd/user/choreographr.service

%changelog
* Sun Aug 09 2026 Choreographr Maintainers <maintainers@choreographr.com> - 0.1.0-1
- Initial 0.1.0 release: prebuilt binaries + systemd user unit.
