#!/usr/bin/env python3

"""An extension of the ARTIQ device DB template script."""

import artiq.frontend.artiq_ddb_template


class PeripheralManager(artiq.frontend.artiq_ddb_template.PeripheralManager):
    """An extension of the ARTIQ device DB template peripheral manager that includes custom peripheral types."""

    def _reserve_explicit_name(self, ty, index):
        self.counts[ty] = max(self.counts[ty], index + 1)
        return "{}{}".format(ty, index)

    def _emit_ttl_out(self, name, channel):
        self.gen("""
            device_db["{name}"] = {{
                "type": "local",
                "module": "artiq.coredevice.ttl",
                "class": "TTLOut",
                "arguments": {{"channel": 0x{channel:06x}}},
            }}""",
                 name=name,
                 channel=channel)

    def _emit_ttl_inout(self, name, channel):
        self.gen("""
            device_db["{name}"] = {{
                "type": "local",
                "module": "artiq.coredevice.ttl",
                "class": "TTLInOut",
                "arguments": {{"channel": 0x{channel:06x}}},
            }}""",
                 name=name,
                 channel=channel)

    def _emit_edge_counter(self, name, channel):
        self.gen("""
            device_db["{name}_counter"] = {{
                "type": "local",
                "module": "artiq.coredevice.edge_counter",
                "class": "EdgeCounter",
                "arguments": {{"channel": 0x{channel:06x}}},
            }}""",
                 name=name,
                 channel=channel)

    def _has_overlapping_dio(self, peripheral):
        entangler_ports = set(peripheral["ports"])
        for candidate in self.master_description["peripherals"]:
            if candidate is peripheral or candidate.get("type") != "dio":
                continue
            if entangler_ports & set(candidate.get("ports", [])):
                return True
        return False

    def process_entangler(self, rtio_offset, peripheral):
        from entangler.config import settings
        from entangler.gateware.io_mapping import build_standalone_ttl_exports
        num_outputs = settings.NUM_OUTPUT_CHANNELS
        num_inputs = settings.NUM_ENTANGLER_INPUT_SIGNALS + settings.NUM_GENERIC_INPUT_SIGNALS

        ports = peripheral["ports"]
        uses_reference = peripheral.get("uses_reference", False)
        running_output = peripheral.get("running_output", False)
        link_eem = peripheral.get("link_eem", None)
        interface_on_lower = peripheral.get("interface_on_lower", True)
        edge_counter = peripheral.get("edge_counter", False)
        logic_mode = peripheral.get("logic_mode", "legacy")
        overlay = peripheral.get("overlay", False) or self._has_overlapping_dio(peripheral)

        assert len(ports) >= 1, 'At least one DIO port is required for DDB generation'
        assert link_eem is None, 'Currently, link eem is not supported in DDB generation'
        if not overlay:
            assert not uses_reference, 'Currently, reference input is not supported for standalone DDB generation'
            assert interface_on_lower, 'Currently, only interface on lower enabled is supported for standalone DDB generation'

        if overlay:
            self.gen("""
                # Entangler overlays an already-declared DIO card. The normal ttlN
                # exports come from the DIO peripheral; this entry only adds the
                # Entangler control RTIO channel.
            """)
            ttl_export_count = 0
        else:
            ttl_exports = build_standalone_ttl_exports(
                ports=ports,
                num_inputs=num_inputs,
                num_outputs=num_outputs,
                edge_counters_enabled=edge_counter,
            )

            self.gen("""
                # Entangler standalone TTL mapping
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
            ttl_export_count = len(ttl_exports)

        driver_module = "entangler.driver"
        driver_class = "Entangler"
        driver_args = '        "channel": 0x{channel:06x},\n        "is_master": True,'
        if logic_mode == "and_nand_test":
            driver_module = "entangler.and_nand_test_driver"
            driver_class = "AndNandTestEntangler"
            driver_args = '        "channel": 0x{channel:06x},'
        elif logic_mode == "atom_photon_parity":
            driver_module = "entangler.atom_photon_parity_driver"
            driver_class = "AtomPhotonParityEntangler"
            driver_args = '        "channel": 0x{channel:06x},'

        entangler_channel = rtio_offset + ttl_export_count
        self.gen("""device_db["{name}"] = {{
    "type": "local",
    "module": "{module}",
    "class": "{class_name}",
    "arguments": {{
{arguments}
    }},
}}""",
                 name=self.get_name("entangler"),
                 module=driver_module,
                 class_name=driver_class,
                 arguments=driver_args.format(channel=entangler_channel))

        return ttl_export_count + 1


if __name__ == "__main__":

    import entangler.gateware.jsondesc


    # Inject custom peripheral manager class
    artiq.frontend.artiq_ddb_template.PeripheralManager = PeripheralManager
    # Inject custom peripherals in JSON schema
    entangler.gateware.jsondesc.inject()

    # Run regular main function
    artiq.frontend.artiq_ddb_template.main()
