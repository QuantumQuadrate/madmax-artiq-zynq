#!/usr/bin/env python3

"""An extension of the ARTIQ device DB template script."""

import artiq.frontend.artiq_ddb_template
from entangler.gateware.io_mapping import build_standalone_ttl_exports


class PeripheralManager(artiq.frontend.artiq_ddb_template.PeripheralManager):
    """ARTIQ DDB template manager with the atom-photon entangler peripheral."""

    def _reserve_explicit_name(self, ty, index):
        self.counts[ty] = max(self.counts[ty], index + 1)
        return "{}{}".format(ty, index)

    def process_entangler(self, rtio_offset, peripheral):
        from entangler.config import settings
        num_outputs = settings.NUM_OUTPUT_CHANNELS
        num_inputs = settings.NUM_ENTANGLER_INPUT_SIGNALS + settings.NUM_GENERIC_INPUT_SIGNALS

        ports = peripheral["ports"]
        mode = peripheral.get("mode", "atom_photon_parity")
        uses_reference = peripheral.get("uses_reference", False)
        running_output = peripheral.get("running_output", False)
        link_eem = peripheral.get("link_eem", None)
        interface_on_lower = peripheral.get("interface_on_lower", True)
        edge_counters_enabled = peripheral.get("edge_counter", False)

        assert len(ports) >= 1, 'At least one DIO port is required for DDB generation'
        assert mode == "atom_photon_parity", 'Only atom_photon_parity mode is enabled'
        assert not uses_reference, 'Atom-photon parity mode does not use references'
        assert link_eem is None, 'Atom-photon parity mode does not use link_eem'
        assert interface_on_lower, 'Only interface_on_lower=true is supported'

        ttl_exports = build_standalone_ttl_exports(
            ports=ports,
            num_inputs=num_inputs,
            num_outputs=num_outputs,
            edge_counters_enabled=edge_counters_enabled,
        )

        self.gen("""
            # Atom-photon parity entangler standalone TTL mapping
            # Physical numbering is by DIO-port order in "ports"; each port contributes
            # ttl[8*n + 0:8*n + 3] for input-side pads and ttl[8*n + 4:8*n + 7] for
            # output-side pads. RTIO channels remain in gateware append order.
        """)

        if running_output:
            self.gen("""
                # Note: running_output reserves one physical output-side pad but does
                # not add a standalone TTL RTIO channel, so there is no extra ttlN export.
            """)

        for export in ttl_exports:
            channel = rtio_offset + export.rtio_channel
            self.gen(
                '# {pad} -> RTIO channel 0x{channel:06x} -> {name} ({kind})',
                pad=export.physical_channel.pad_label,
                channel=channel,
                name=export.device_name,
                kind=export.device_kind,
            )
            if export.device_kind == "counter":
                self.gen("""
                    device_db["{name}"] = {{
                        "type": "local",
                        "module": "artiq.coredevice.edge_counter",
                        "class": "{class_name}",
                        "arguments": {{"channel": 0x{channel:06x}}},
                    }}""",
                         name=export.device_name,
                         class_name=export.device_class,
                         channel=channel)
                continue

            ttl_index = export.physical_channel.exported_ttl_index
            self.gen("""
                device_db["{name}"] = {{
                    "type": "local",
                    "module": "artiq.coredevice.ttl",
                    "class": "{class_name}",
                    "arguments": {{"channel": 0x{channel:06x}}},
                }}""",
                     name=self._reserve_explicit_name("ttl", ttl_index),
                     class_name=export.device_class,
                     channel=channel)

        self.gen("""
            device_db["{name}"] = {{
                "type": "local",
                "module": "entangler.atom_photon_driver",
                "class": "AtomPhotonEntangler",
                "arguments": {{
                    "channel": 0x{channel:06x}
                }},
            }}""",
                 name=self.get_name("entangler"),
                 channel=rtio_offset + len(ttl_exports))

        return len(ttl_exports) + 1


if __name__ == "__main__":

    import entangler.gateware.jsondesc


    # Inject custom peripheral manager class
    artiq.frontend.artiq_ddb_template.PeripheralManager = PeripheralManager
    # Inject custom peripherals in JSON schema
    entangler.gateware.jsondesc.inject()

    # Run regular main function
    artiq.frontend.artiq_ddb_template.main()
