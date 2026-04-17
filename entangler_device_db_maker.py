#!/usr/bin/env python3

"""An extension of the ARTIQ device DB template script."""

import artiq.frontend.artiq_ddb_template


class PeripheralManager(artiq.frontend.artiq_ddb_template.PeripheralManager):
    """An extension of the ARTIQ device DB template peripheral manager that includes custom peripheral types."""

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

    def process_entangler(self, rtio_offset, peripheral):
        from entangler.config import settings
        num_outputs = settings.NUM_OUTPUT_CHANNELS
        num_inputs = settings.NUM_ENTANGLER_INPUT_SIGNALS + settings.NUM_GENERIC_INPUT_SIGNALS

        ports = peripheral["ports"]
        uses_reference = peripheral.get("uses_reference", False)
        running_output = peripheral.get("running_output", False)
        link_eem = peripheral.get("link_eem", None)
        interface_on_lower = peripheral.get("interface_on_lower", True)
        edge_counter = peripheral.get("edge_counter", False)

        assert len(ports) in (1, 2), 'Currently, only one or two ports are supported for DDB generation'
        assert not uses_reference, 'Currently, reference input is not supported for DDB generation'
        assert link_eem is None, 'Currently, link eem is not supported in DDB generation'
        assert interface_on_lower, 'Currently, only interface on lower enabled is supported for DDB generation'

        total_dio_pads = len(ports) * 8
        reserved_running_outputs = 1 if running_output else 0
        leftover_outputs = total_dio_pads - num_outputs - reserved_running_outputs - num_inputs
        if leftover_outputs < 0:
            raise ValueError(
                "Insufficient DIO pads for requested Entangler device-db layout"
            )

        channel = rtio_offset

        for _ in range(num_outputs):
            self._emit_ttl_out(self.get_name("ttl"), channel)
            channel += 1

        for _ in range(num_inputs):
            name = self.get_name("ttl")
            self._emit_ttl_inout(name, channel)
            channel += 1
            if edge_counter:
                self._emit_edge_counter(name, channel)
                channel += 1

        self.gen("""
            device_db["{name}"] = {{
                "type": "local",
                "module": "entangler.driver",
                "class": "Entangler",
                "arguments": {{
                    "channel": 0x{channel:06x},
                    "is_master": True,
                }},
            }}""",
                 name=self.get_name("entangler"),
                 channel=channel)
        channel += 1

        for _ in range(leftover_outputs):
            self._emit_ttl_out(self.get_name("ttl"), channel)
            channel += 1

        return channel - rtio_offset


if __name__ == "__main__":

    import entangler.gateware.jsondesc


    # Inject custom peripheral manager class
    artiq.frontend.artiq_ddb_template.PeripheralManager = PeripheralManager
    # Inject custom peripherals in JSON schema
    entangler.gateware.jsondesc.inject()

    # Run regular main function
    artiq.frontend.artiq_ddb_template.main()
