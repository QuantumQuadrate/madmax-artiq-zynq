import logging

from migen import Mux
from migen import Signal

import artiq.gateware.eem as artiq_eem
import artiq.gateware.eem_7series as artiq_eem_7series
from artiq.gateware.rtio.phy import edge_counter
from artiq.gateware.rtio.phy import ttl_serdes_7series, ttl_simple
import entangler.gateware.eem_7series as upstream_eem_7series
import entangler.kasli_generic as entangler_kasli
import entangler.phy
from entangler.config import settings as entangler_settings


_LOGGER = logging.getLogger(__name__)
_ORIGINAL_ADD_PERIPHERALS = None
_ORIGINAL_DIO_PROCESSOR = None


def _dio_direction(peripheral, physical_index):
    key = "bank_direction_low" if physical_index < 4 else "bank_direction_high"
    return peripheral[key]


def _recorded_dio_ports(target):
    if not hasattr(target, "_entangler_dio_records"):
        target._entangler_dio_records = {}
    return target._entangler_dio_records


def peripheral_dio(module, peripheral, **kwargs):
    if len(peripheral["ports"]) != 1:
        raise ValueError("wrong number of ports")

    ttl_classes = {
        "input": ttl_serdes_7series.InOut_8X,
        "output": ttl_serdes_7series.Output_8X,
        "clkgen": ttl_simple.ClockGen,
    }
    edge_counter_cls = (
        edge_counter.SimpleEdgeCounter if peripheral.get("edge_counter", False) else None
    )

    eem = peripheral["ports"][0]
    artiq_eem.DIO.add_extension(module, eem, **kwargs)

    dci = kwargs.get("iostandard", artiq_eem.default_iostandard)(eem).name == "LVDS"
    records = {}
    phys = []
    for physical_index in range(8):
        direction = _dio_direction(peripheral, physical_index)
        pads = module.platform.request(f"dio{eem}", physical_index)
        phy = ttl_classes[direction](pads.p, pads.n, dci=dci)
        phys.append((physical_index, phy))
        module.submodules += phy
        channel = len(module.rtio_channels)
        module.rtio_channels.append(entangler_kasli.rtio.Channel.from_phy(phy))
        records[physical_index] = {
            "direction": direction,
            "pads": pads,
            "phy": phy,
            "channel": channel,
        }

    if edge_counter_cls is not None:
        for physical_index, phy in phys:
            state = getattr(phy, "input_state", None)
            if state is not None:
                counter = edge_counter_cls(state)
                module.submodules += counter
                channel = len(module.rtio_channels)
                module.rtio_channels.append(
                    entangler_kasli.rtio.Channel.from_phy(counter)
                )
                records[physical_index]["counter_channel"] = channel

    _recorded_dio_ports(module)[eem] = records


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


def _phy_cls_for_logic_mode(peripheral):
    logic_mode = peripheral.get("logic_mode", "legacy")
    if logic_mode == "and_nand_test":
        from entangler.and_nand_test_phy import AndNandTestEntangler

        return AndNandTestEntangler
    if logic_mode == "atom_photon_parity":
        from entangler.atom_photon_phy import AtomPhotonParity

        return AtomPhotonParity
    if logic_mode == "legacy":
        return entangler.phy.Entangler
    raise ValueError(f"Unsupported entangler logic_mode {logic_mode!r}")


def _recorded_pool(target, ports, physical_indices, direction):
    records_by_port = _recorded_dio_ports(target)
    pool = []
    for eem in ports:
        records = records_by_port.get(eem)
        if records is None:
            return None
        for physical_index in physical_indices:
            record = records[physical_index]
            if record["direction"] != direction:
                raise ValueError(
                    "Entangler overlay requires dio{}[{}] to be {}, but the DIO "
                    "peripheral configured it as {}".format(
                        eem, physical_index, direction, record["direction"]
                    )
                )
            pool.append((eem, physical_index, record))
    return pool


def _overlay_output_overrides(target, allocated_outputs):
    output_overrides = []
    for eem, physical_index, record in allocated_outputs:
        channel_index = record["channel"]
        channel = target.rtio_channels[channel_index]
        if len(channel.overrides) < 2:
            raise ValueError(
                f"dio{eem}[{physical_index}] does not expose RTIO override signals"
            )

        phy_override_en, phy_override_o = channel.overrides[:2]
        moninj_override_en = Signal()
        moninj_override_o = Signal()
        helper_override_en = Signal()
        helper_override_o = Signal()

        target.comb += [
            phy_override_en.eq(helper_override_en | moninj_override_en),
            phy_override_o.eq(Mux(helper_override_en, helper_override_o, moninj_override_o)),
        ]

        # Keep ARTIQ monitor/injection alive without letting it overwrite the
        # helper override. MonInj is constructed later from rtio_channels, so
        # replacing the channel overrides here makes it drive these proxy signals.
        channel.overrides = [moninj_override_en, moninj_override_o, *channel.overrides[2:]]
        output_overrides.append([helper_override_en, helper_override_o])

    return output_overrides


def _should_overlay_entangler(target, peripheral):
    if peripheral.get("link_eem") is not None:
        if peripheral.get("overlay", False):
            raise ValueError("Entangler overlay does not support link_eem")
        return False

    ports = peripheral["ports"]
    records_by_port = _recorded_dio_ports(target)
    has_recorded_ports = all(port in records_by_port for port in ports)
    if peripheral.get("overlay", False) and not has_recorded_ports:
        raise ValueError(
            "Entangler overlay requires matching DIO peripherals to appear before "
            "the entangler peripheral for ports {}".format(ports)
        )
    return has_recorded_ports


def _add_overlay_entangler(target, peripheral):
    ports = peripheral["ports"]
    num_outputs = entangler_settings.NUM_OUTPUT_CHANNELS
    num_entangler_inputs = entangler_settings.NUM_ENTANGLER_INPUT_SIGNALS
    num_generic_inputs = entangler_settings.NUM_GENERIC_INPUT_SIGNALS
    num_total_inputs = num_entangler_inputs + num_generic_inputs
    num_reference_inputs = 1 if peripheral.get("uses_reference", False) else 0
    num_running_outputs = 1 if peripheral.get("running_output", False) else 0

    input_pool = _recorded_pool(target, ports, range(4), "input")
    output_pool = _recorded_pool(target, ports, range(4, 8), "output")
    if input_pool is None or output_pool is None:
        raise ValueError("Entangler overlay could not find matching DIO records")

    needed_inputs = num_total_inputs + num_reference_inputs
    needed_outputs = num_outputs + num_running_outputs
    if needed_inputs > len(input_pool):
        raise ValueError("Insufficient DIO input pads for Entangler overlay")
    if needed_outputs > len(output_pool):
        raise ValueError("Insufficient DIO output pads for Entangler overlay")

    allocated_inputs = input_pool[:num_total_inputs]
    reference_input = input_pool[num_total_inputs] if num_reference_inputs else None
    allocated_outputs = output_pool[:needed_outputs]

    input_phys = [
        record["phy"].rtlink.i
        for _, _, record in allocated_inputs[:num_entangler_inputs]
    ]
    input_states = [
        getattr(record["phy"], "input_state", None)
        for _, _, record in allocated_inputs
    ]
    output_overrides = _overlay_output_overrides(target, allocated_outputs)

    reference_phy = reference_input[2]["phy"].rtlink.i if reference_input else None
    phy_cls = _phy_cls_for_logic_mode(peripheral)
    logic_mode = peripheral.get("logic_mode", "legacy")

    if logic_mode == "and_nand_test":
        phy = phy_cls(
            output_pads=None,
            passthrough_sigs=None,
            input_phys=input_phys,
            input_states=input_states[:2],
            output_overrides=output_overrides,
            simulate=False,
        )
    elif logic_mode == "atom_photon_parity":
        phy = phy_cls(
            core_link_pads=None,
            output_pads=None,
            passthrough_sigs=None,
            input_phys=input_phys,
            input_states=input_states[:2],
            output_overrides=output_overrides,
            simulate=False,
        )
    else:
        phy = phy_cls(
            core_link_pads=None,
            output_pads=None,
            passthrough_sigs=None,
            input_phys=input_phys,
            reference_phy=reference_phy,
            output_overrides=output_overrides,
            simulate=False,
        )

    target.submodules += phy
    target.rtio_channels.append(entangler_kasli.rtio.Channel.from_phy(phy))
    _LOGGER.info(
        "Added Entangler overlay on DIO ports %s at RTIO channel %i",
        ports,
        len(target.rtio_channels) - 1,
    )


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
    if _should_overlay_entangler(module, peripheral):
        _add_overlay_entangler(module, peripheral)
        return

    if _split_single_dio_entangler(peripheral):
        _LOGGER.info(
            "Using split-bank Entangler mapping on dio%s: inputs on [0..3], outputs on [4..7]",
            peripheral["ports"][0],
        )
        _add_single_dio_bank_split_entangler(module, peripheral)
        return

    entangler_kasli.peripheral_entangler(module, peripheral, **kwargs)


def add_peripherals(module, peripherals, **kwargs):
    _recorded_dio_ports(module)
    return _ORIGINAL_ADD_PERIPHERALS(module, peripherals, **kwargs)


def inject():
    global _ORIGINAL_ADD_PERIPHERALS
    global _ORIGINAL_DIO_PROCESSOR

    upstream_eem_7series.inject()
    if _ORIGINAL_ADD_PERIPHERALS is None:
        _ORIGINAL_ADD_PERIPHERALS = artiq_eem_7series.add_peripherals
    if _ORIGINAL_DIO_PROCESSOR is None:
        _ORIGINAL_DIO_PROCESSOR = artiq_eem_7series.peripheral_processors["dio"]

    artiq_eem_7series.add_peripherals = add_peripherals
    artiq_eem_7series.peripheral_processors["dio"] = peripheral_dio
    artiq_eem_7series.peripheral_processors["entangler"] = peripheral_entangler
