#!/usr/bin/env python3

"""An extension of the ARTIQ device DB template script."""

from itertools import count

import artiq.frontend.artiq_ddb_template


class PeripheralManager(artiq.frontend.artiq_ddb_template.PeripheralManager):
    """An extension of the ARTIQ device DB template peripheral manager that includes custom peripheral types."""

    def process_entangler(self, rtio_offset, peripheral):
        from entangler.config import settings
        num_outputs = settings.NUM_OUTPUT_CHANNELS
        num_inputs = settings.NUM_ENTANGLER_INPUT_SIGNALS + settings.NUM_GENERIC_INPUT_SIGNALS

        ports = peripheral["ports"]
        uses_reference = peripheral.get("uses_reference", False)
        running_output = peripheral.get("running_output", False)
        link_eem = peripheral.get("link_eem", None)
        interface_on_lower = peripheral.get("interface_on_lower", True)

        assert len(ports) == 2, 'Currently, only two ports is supported for DDB generation'
        assert not uses_reference, 'Currently, reference input is not supported for DDB generation'
        assert link_eem is None, 'Currently, link eem is not supported in DDB generation'
        assert interface_on_lower, 'Currently, only interface on lower enabled is supported for DDB generation'

        channel = count(0)

        for i in range(num_outputs):
            if running_output and i == (num_outputs - 1):
                # skip this channel
                continue
            self.gen("""
                device_db["{name}"] = {{
                    "type": "local",
                    "module": "artiq.coredevice.ttl",
                    "class": "TTLOut",
                    "arguments": {{"channel": 0x{channel:06x}}},
                }}""",
                     name=self.get_name('ttl'),
                     channel=rtio_offset + next(channel))

        for _ in range(num_inputs):
            name = self.get_name('ttl')
            self.gen("""
                device_db["{name}"] = {{
                    "type": "local",
                    "module": "artiq.coredevice.ttl",
                    "class": "TTLInOut",
                    "arguments": {{"channel": 0x{channel:06x}}},
                }}""",
                     name=name,
                     channel=rtio_offset + next(channel))
            self.gen("""
                device_db["{name}_counter"] = {{
                    "type": "local",
                    "module": "artiq.coredevice.edge_counter",
                    "class": "EdgeCounter",
                    "arguments": {{"channel": 0x{channel:06x}}},
                }}""",
                     name=name,
                     channel=rtio_offset + next(channel))

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
                 channel=rtio_offset + next(channel))

        return next(channel)


if __name__ == "__main__":

    import entangler.gateware.jsondesc


    # Inject custom peripheral manager class
    artiq.frontend.artiq_ddb_template.PeripheralManager = PeripheralManager
    # Inject custom peripherals in JSON schema
    entangler.gateware.jsondesc.inject()

    # Run regular main function
    artiq.frontend.artiq_ddb_template.main()
