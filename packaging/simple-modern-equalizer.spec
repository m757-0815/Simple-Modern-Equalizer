%global debug_package %{nil}
Name:           simple-modern-equalizer
Version:        %{app_version}
Release:        2%{?dist}
Summary:        System-wide graphical audio equalizer
License:        GPL-3.0-only AND OFL-1.1
URL:            https://github.com/m757-0815/Simple-Modern-Equalizer
BuildRequires:  desktop-file-utils
Requires:       hicolor-icon-theme
Requires:       pipewire

%description
Adjust system audio with tone controls, a parametric equalizer,
headphone presets, and live visualizations.

%install
install -Dm755 %{staged_app}/simple-modern-equalizer %{buildroot}%{_libexecdir}/%{name}/simple-modern-equalizer
install -Dm755 %{staged_app}/resonanced %{buildroot}%{_libexecdir}/%{name}/resonanced
install -d %{buildroot}%{_bindir}
ln -s %{_libexecdir}/%{name}/simple-modern-equalizer %{buildroot}%{_bindir}/simple-modern-equalizer
install -Dm644 %{staged_app}/share/applications/%{name}.desktop %{buildroot}%{_datadir}/applications/%{name}.desktop
install -Dm644 %{staged_app}/share/icons/hicolor/256x256/apps/%{name}.png %{buildroot}%{_datadir}/icons/hicolor/256x256/apps/%{name}.png
install -Dm644 %{staged_app}/LICENSE %{buildroot}%{_licensedir}/%{name}/LICENSE
install -Dm644 %{staged_app}/Inter-OFL.txt %{buildroot}%{_licensedir}/%{name}/Inter-OFL.txt
install -Dm644 %{staged_app}/THIRD_PARTY_NOTICES.md %{buildroot}%{_docdir}/%{name}/THIRD_PARTY_NOTICES.md

%check
desktop-file-validate %{buildroot}%{_datadir}/applications/%{name}.desktop
test -s %{buildroot}%{_datadir}/icons/hicolor/256x256/apps/%{name}.png

%files
%{_bindir}/simple-modern-equalizer
%{_libexecdir}/%{name}/
%{_datadir}/applications/%{name}.desktop
%{_datadir}/icons/hicolor/256x256/apps/%{name}.png
%license %{_licensedir}/%{name}/
%doc %{_docdir}/%{name}/
