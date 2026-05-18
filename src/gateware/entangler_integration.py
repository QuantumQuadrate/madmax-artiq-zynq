import logging

from migen import Signal

import artiq.gateware.eem_7series as artiq_eem_7series
from artiq.gateware.rtio.phy import ttl_serdes_7series, ttl_simple
import entangler.gateware.eem_7series as upstream_eem_7series
import entangler.kasli_generic as entangler_kasli
import entangler.phy
from entangler.config import settings as entangler_settings


_LOGGER = logging.getLogger(__name__)


def _split_single_dio_entangler(peripheral):
    if peripheral.get("logic_mode", "legacy") != "legacy":
        return False
    if peripheral.get("link_eem") is not None:
        return False
    if peripheral.get("uses_reference", False):
        return False
    if peripheral.get("running_output", False):
        return False
    if len(peripheral["ports"]) != 1:
        return False

    num_outputs = entangler_settings.NUM_OUTPUT_CHANNELS
    num_inputs = (
        entangler_settings.NUM_ENTANGLER_INPUT_SIGNALS
        + entangler_settings.NUM_GENERIC_INPUT_SIGNALS
    )
    return num_outputs <= 4 and num_inputs <= 4


def _request_dio_pads(target, eem, pad_indices):
    return [target.platform.request(f"dio{eem}", i) for i in pad_indices]


def _add_single_dio_bank_split_entangler(target, peripheral):
    eem = peripheral["ports"][0]
    entangler_kasli.EntanglerEEM.add_extension(target, [eem])

    if entangler_kasli._ARTIQ_MAJOR_VERSION >= 6:
        io_class = {
            "input": ttl_serdes_7series.InOut_8X,
            "output": ttl_simple.Output,
        }
    else:
        io_class = {
            "input": ttl_serdes_7series.Input_8X,
            "output": ttl_simple.Output,
        }

    num_outputs = entangler_settings.NUM_OUTPUT_CHANNELS
    num_entangler_inputs = entangler_settings.NUM_ENTANGLER_INPUT_SIGNALS
    num_generic_inputs = entangler_settings.NUM_GENERIC_INPUT_SIGNALS
    num_total_inputs = num_entangler_inputs + num_generic_inputs

    output_indices = list(range(4, 4 + num_outputs))
    input_indices = list(range(num_total_inputs))
    used_indices = set(output_indices + input_indices)
    leftover_indices = [i for i in range(8) if i not in used_indices]

    output_pads = []
    output_sigs = [Signal() for _ in range(num_outputs)]
    for i, pads in enumerate(_request_dio_pads(target, eem, output_indices)):
        output_pads.append(pads)
        phy = io_class["output"](output_sigs[i])
        target.submodules += phy
        target.rtio_channels.append(entangler_kasli.rtio.Channel.from_phy(phy))

    _LOGGER.info(
        "Configured Entangler outputs on dio%s[%s] for RTIO channels %i -> %i",
        eem,
        ", ".join(str(i) for i in output_indices),
        len(target.rtio_channels) - num_outputs,
        len(target.rtio_channels) - 1,
    )

    input_phys = []
    for i, pads in enumerate(_request_dio_pads(target, eem, input_indices)):
        phy = io_class["input"](pads.p, pads.n)
        target.submodules += phy
        if i < num_entangler_inputs:
            input_phys.append(phy.rtlink.i)
        target.rtio_channels.append(entangler_kasli.rtio.Channel.from_phy(phy))

        edge_counter_cls = (
            entangler_kasli.EDGE_COUNTER_CLS if peripheral.get("edge_counter") else None
        )
        if edge_counter_cls is not None:
            state = getattr(phy, "input_state", None)
            if state is not None:
                counter = edge_counter_cls(state)
                target.submodules += counter
                target.rtio_channels.append(
                    entangler_kasli.rtio.Channel.from_phy(counter)
                )

    _LOGGER.info(
        "Configured Entangler inputs on dio%s[%s] for RTIO channels %i -> %i",
        eem,
        ", ".join(str(i) for i in input_indices),
        len(target.rtio_channels) - num_total_inputs,
        len(target.rtio_channels) - 1,
    )

    phy = entangler.phy.Entangler(
        core_link_pads=None,
        output_pads=output_pads,
        passthrough_sigs=output_sigs,
        input_phys=input_phys,
        reference_phy=None,
        simulate=False,
    )
    target.submodules += phy
    target.rtio_channels.append(entangler_kasli.rtio.Channel.from_phy(phy))

    for pads in _request_dio_pads(target, eem, leftover_indices):
        phy = io_class["output"](pads.p, pads.n)
        target.submodules += phy
        target.rtio_channels.append(entangler_kasli.rtio.Channel.from_phy(phy))


def peripheral_entangler(module, peripheral, **kwargs):
    if _split_single_dio_entangler(peripheral):
        _LOGGER.info(
            "Using split-bank Entangler mapping on dio%s: inputs on [0..3], outputs on [4..7]",
            peripheral["ports"][0],
        )
        _add_single_dio_bank_split_entangler(module, peripheral)
        return

    entangler_kasli.peripheral_entangler(module, peripheral, **kwargs)


def inject():
    upstream_eem_7series.inject()
    artiq_eem_7series.peripheral_processors["entangler"] = peripheral_entangler
