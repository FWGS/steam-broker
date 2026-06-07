## steam-broker

Small program that talks to Steam on behalf of another programs over TCP. Implementation of Xash3D FWGS's steam broker protocol: https://github.com/FWGS/xash3d-fwgs/blob/master/Documentation/extensions/steam-broker.md

Currently, only supports Linux and intended to run as a systemd user service.

By default it listens on `127.0.0.1:27420`. Pass a different address as the first
argument to override it, e.g. `steam-broker 0.0.0.0:27420`. To change it for the
systemd unit, edit the address in the `ExecStart=` line of `steam-broker.service`.

