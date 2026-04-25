"""Smoke test for the atom-photon parity Entangler gateware.

Run this against the generated atom-photon ``device_db.py`` after flashing the
matching ``build/boot.bin``. It is intentionally conservative: it does not
require loopback cables or photon pulses. With no input events, the Entangler
should finish one attempt with the "neither" terminal path.

Useful physical checks while it runs:

- ``ttl4`` is ``dio0[4]`` / output bit 0.
- ``ttl5`` is ``dio0[5]`` / output bit 1.
- ``ttl12`` is ``dio1[4]`` / output bit 4.
- ``ttl15`` is ``dio1[7]`` / output bit 7.
- ``ttl0`` and ``ttl1`` are the two SPCM inputs on ``dio0[0]`` and ``dio0[1]``.
"""

from artiq.experiment import EnvExperiment, kernel
from artiq.language.core import delay_mu


class AtomPhotonGatewareSmoke(EnvExperiment):
    """Basic hardware smoke test for the atom-photon parity gateware."""

    def build(self):
        self.setattr_device("core")
        self.setattr_device("entangler0")

        self.setattr_device("ttl0")
        self.setattr_device("ttl1")
        self.setattr_device("ttl4")
        self.setattr_device("ttl5")
        self.setattr_device("ttl12")
        self.setattr_device("ttl15")
        self.setattr_device("led0")

    @kernel
    def run(self):
        self.core.reset()
        self.core.break_realtime()

        self.entangler0.init()
        self.entangler0.clear()
        delay_mu(10000)

        self.ttl0.input()
        self.ttl1.input()
        self.core.break_realtime()

        # Direct TTL pulses verify that the generated device_db.py matches the
        # physical output mapping before the Entangler takes over those outputs.
        self.ttl4.pulse_mu(1000)
        delay_mu(2000)
        self.ttl5.pulse_mu(1000)
        delay_mu(2000)
        self.ttl12.pulse_mu(1000)
        delay_mu(2000)
        self.ttl15.pulse_mu(1000)
        delay_mu(2000)
        self.led0.pulse_mu(1000)
        self.core.break_realtime()

        self.configure_one_attempt_no_photon()
        finished_at, done_word = self.entangler0.run_mu()
        self.core.break_realtime()

        status = self.entangler0.get_status()
        outcome = self.entangler0.get_outcome()
        done_reason = self.entangler0.get_done_reason()
        attempts = self.entangler0.get_attempts_completed()
        spcm0_ts = self.entangler0.get_spcm0_timestamp_mu()
        spcm1_ts = self.entangler0.get_spcm1_timestamp_mu()
        chosen_ts = self.entangler0.get_chosen_timestamp_mu()
        self.entangler0.set_config(0)

        print("atom-photon smoke finished_at", finished_at)
        print("atom-photon smoke done_word", done_word)
        print("atom-photon smoke status", status)
        print("atom-photon smoke outcome", outcome)
        print("atom-photon smoke done_reason", done_reason)
        print("atom-photon smoke attempts", attempts)
        print("atom-photon smoke spcm0_ts", spcm0_ts)
        print("atom-photon smoke spcm1_ts", spcm1_ts)
        print("atom-photon smoke chosen_ts", chosen_ts)

    @kernel
    def configure_one_attempt_no_photon(self):
        self.entangler0.clear()

        self.entangler0.write_register(0x02, 1)      # N_ATTEMPTS
        self.entangler0.write_register(0x03, 256)    # ATTEMPT_PERIOD_MU
        self.entangler0.write_register(0x04, 16)     # FORT_OFF_MU
        self.entangler0.write_register(0x05, 160)    # FORT_ON_MU
        self.entangler0.write_register(0x06, 32)     # EXCITATION_START_MU
        self.entangler0.write_register(0x07, 96)     # EXCITATION_STOP_MU
        self.entangler0.write_register(0x08, 64)     # PHOTON_GATE_START_MU
        self.entangler0.write_register(0x09, 128)    # PHOTON_GATE_STOP_MU

        # STOP_FAIL for neither and both: with no loopback, one attempt should
        # finish instead of retrying forever.
        self.entangler0.write_register(0x0A, 0x11)   # BRANCH_POLICIES
        self.entangler0.write_register(0x11, 0)      # BRANCH0_ACTION_COUNT
        self.entangler0.write_register(0x12, 0)      # BRANCH1_ACTION_COUNT

        self.entangler0.set_config(1)
