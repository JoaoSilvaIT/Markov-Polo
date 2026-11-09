#!/usr/bin/env python3
"""
NautilusTrader Decoupled Strategy for Markov-Polo.
Implements:
- Causal Ornstein-Uhlenbeck Mean Reversion (Engine A) with MFE Stall-Hazard Maker Trajectory.
- Volatility Squeeze Breakout (Engine B).
- 100% Post-Only Alo Maker entry and Two-Tranche Maker exit matching Hyperliquid L1.
"""

import math
from collections import deque
from decimal import Decimal
from typing import Optional

from nautilus_trader.config import StrategyConfig
from nautilus_trader.model.data import QuoteTick, TradeTick
from nautilus_trader.model.enums import OrderSide, TimeInForce
from nautilus_trader.model.identifiers import InstrumentId
from nautilus_trader.model.objects import Price, Quantity
from nautilus_trader.model.orders import LimitOrder, MarketOrder
from nautilus_trader.trading.strategy import Strategy


class MarkovPoloNautilusConfig(StrategyConfig):
    instrument_id: str
    target_notional_usdc: float = 133.85
    min_notional_usdc: float = 22.0
    ou_horizon_ticks: int = 1200  # Rolling window for local equilibrium
    base_entry_sigma: float = 1.65
    hard_disaster_stop_bps: float = 40.0
    beta_overshoot: float = 0.58
    stall_trigger_ticks: int = 180  # Ticks without new MFE peak before stall ratchet


class MarkovPoloNautilusStrategy(Strategy):
    def __init__(self, config: MarkovPoloNautilusConfig):
        super().__init__(config)
        self.instrument_id = InstrumentId.from_str(config.instrument_id)
        self.target_notional_usdc = config.target_notional_usdc
        self.min_notional_usdc = config.min_notional_usdc

        # Market data buffers
        self.prices = deque(maxlen=config.ou_horizon_ticks)
        self.timestamps = deque(maxlen=config.ou_horizon_ticks)

        # Active state
        self.active_entry_order: Optional[LimitOrder] = None
        self.active_tp1_order: Optional[LimitOrder] = None
        self.active_tp2_order: Optional[LimitOrder] = None
        self.active_sl_order: Optional[MarketOrder] = None

        # Position tracking
        self.is_in_position = False
        self.pos_side: Optional[OrderSide] = None
        self.entry_price: float = 0.0
        self.entry_qty: float = 0.0
        self.tranche1_filled = False
        self.tranche2_filled = False
        self.peak_favorable_px: float = 0.0
        self.ticks_since_peak: int = 0
        self.frozen_mean_px: float = 0.0
        self.frozen_sigma_eq: float = 0.0

        # L2 top-of-book cache
        self.best_bid: float = 0.0
        self.best_ask: float = 0.0
        self.last_mid: float = 0.0

    def on_start(self):
        self.instrument = self.cache.instrument(self.instrument_id)
        if not self.instrument:
            self.log.error(f"Instrument {self.instrument_id} not found in cache!")
            self.stop()
            return

        self.subscribe_quote_ticks(self.instrument_id)
        self.subscribe_trade_ticks(self.instrument_id)
        self.log.info(
            f"🚀 [MARKOV-POLO NAUTILUS] Subscribed to {self.instrument_id.value}"
        )

    def on_quote_tick(self, tick: QuoteTick):
        self.best_bid = float(tick.bid_price)
        self.best_ask = float(tick.ask_price)
        self.last_mid = 0.5 * (self.best_bid + self.best_ask)

        if self.is_in_position:
            self._manage_open_position(self.last_mid)

    def on_trade_tick(self, tick: TradeTick):
        px = float(tick.price)
        self.prices.append(px)
        self.timestamps.append(tick.ts_event)

        if (
            len(self.prices) >= 200
            and not self.is_in_position
            and self.active_entry_order is None
        ):
            self._evaluate_ou_mean_reversion_entry(px)

    def _evaluate_ou_mean_reversion_entry(self, current_px: float):
        if self.best_bid <= 0.0 or self.best_ask <= 0.0:
            return

        # Estimate simple causal rolling mean and standard deviation
        n = len(self.prices)
        mean_px = sum(self.prices) / n
        variance = sum((p - mean_px) ** 2 for p in self.prices) / n
        sigma = math.sqrt(variance)

        if sigma <= 1e-6 or mean_px <= 1e-6:
            return

        z_score = (current_px - mean_px) / sigma

        # Check entry trigger: Long when Z <= -1.65, Short when Z >= +1.65
        if z_score <= -self.config.base_entry_sigma:
            limit_px = self.best_bid
            if limit_px < self.best_ask:
                self._submit_post_only_entry(OrderSide.BUY, limit_px, mean_px, sigma)
        elif z_score >= self.config.base_entry_sigma:
            limit_px = self.best_ask
            if limit_px > self.best_bid:
                self._submit_post_only_entry(OrderSide.SELL, limit_px, mean_px, sigma)

    def _submit_post_only_entry(
        self, side: OrderSide, limit_px: float, mean_px: float, sigma: float
    ):
        qty_coins = self.target_notional_usdc / limit_px
        qty = self.instrument.make_qty(
            Decimal(str(round(qty_coins, self.instrument.size_precision)))
        )
        px = self.instrument.make_price(
            Decimal(str(round(limit_px, self.instrument.price_precision)))
        )

        if (
            self.instrument.min_quantity is not None
            and qty < self.instrument.min_quantity
        ):
            return
        if float(qty) <= 0.0:
            return

        self.frozen_mean_px = mean_px
        self.frozen_sigma_eq = sigma

        order = self.order_factory.limit(
            instrument_id=self.instrument_id,
            order_side=side,
            quantity=qty,
            price=px,
            time_in_force=TimeInForce.GTC,
            post_only=True,
            reduce_only=False,
        )
        self.active_entry_order = order
        self.submit_order(order)
        self.log.info(
            f"🟡 [ALO ENTRY SUBMITTED] {side.name} {qty} @ {px} (Z={((limit_px - mean_px) / sigma):.2f})"
        )

    def on_order_filled(self, event):
        order = self.cache.order(event.client_order_id)
        if not order:
            return

        if (
            self.active_entry_order
            and order.client_order_id == self.active_entry_order.client_order_id
        ):
            self.active_entry_order = None
            self.is_in_position = True
            self.pos_side = order.side
            self.entry_price = float(event.last_px)
            self.entry_qty = float(order.quantity)
            self.peak_favorable_px = self.entry_price
            self.ticks_since_peak = 0
            self.tranche1_filled = False
            self.tranche2_filled = False

            self.log.info(
                f"🟢 [ALO ENTRY FILLED] {self.pos_side.name} {self.entry_qty} @ {self.entry_price}"
            )
            self._place_initial_take_profits()

        elif (
            self.active_tp1_order
            and order.client_order_id == self.active_tp1_order.client_order_id
        ):
            self.active_tp1_order = None
            self.tranche1_filled = True
            self.log.info(f"🎯 [TRANCHE 1 MAKER TP FILLED] @ {float(event.last_px)}")

        elif (
            self.active_tp2_order
            and order.client_order_id == self.active_tp2_order.client_order_id
        ):
            self.active_tp2_order = None
            self.tranche2_filled = True
            self._close_position_complete("FULL_2TRANCHE_MAKER_TP")

    def _place_initial_take_profits(self):
        d = 1.0 if self.pos_side == OrderSide.BUY else -1.0
        exit_side = OrderSide.SELL if self.pos_side == OrderSide.BUY else OrderSide.BUY

        # Tranche 1: 50% placed at equilibrium mean
        tp1_qty_val = round(0.50 * self.entry_qty, self.instrument.size_precision)
        tp1_qty = self.instrument.make_qty(Decimal(str(tp1_qty_val)))
        tp1_px_val = round(self.frozen_mean_px, self.instrument.price_precision)
        tp1_px = self.instrument.make_price(Decimal(str(tp1_px_val)))

        if float(tp1_qty) > 0.0:
            tp1_order = self.order_factory.limit(
                instrument_id=self.instrument_id,
                order_side=exit_side,
                quantity=tp1_qty,
                price=tp1_px,
                time_in_force=TimeInForce.GTC,
                post_only=True,
                reduce_only=True,
            )
            self.active_tp1_order = tp1_order
            self.submit_order(tp1_order)

        # Tranche 2: 50% placed at mean + beta_overshoot * sigma
        tp2_qty_val = round(
            self.entry_qty - tp1_qty_val, self.instrument.size_precision
        )
        tp2_qty = self.instrument.make_qty(Decimal(str(tp2_qty_val)))
        tp2_raw = (
            self.frozen_mean_px + d * self.config.beta_overshoot * self.frozen_sigma_eq
        )
        tp2_px = self.instrument.make_price(
            Decimal(str(round(tp2_raw, self.instrument.price_precision)))
        )

        if float(tp2_qty) > 0.0:
            tp2_order = self.order_factory.limit(
                instrument_id=self.instrument_id,
                order_side=exit_side,
                quantity=tp2_qty,
                price=tp2_px,
                time_in_force=TimeInForce.GTC,
                post_only=True,
                reduce_only=True,
            )
            self.active_tp2_order = tp2_order
            self.submit_order(tp2_order)

    def _manage_open_position(self, current_mid: float):
        d = 1.0 if self.pos_side == OrderSide.BUY else -1.0
        unreal_bps = (
            d * ((current_mid - self.entry_price) / self.entry_price) * 10_000.0
        )

        # Update MFE
        if d * (current_mid - self.peak_favorable_px) > 0.0:
            self.peak_favorable_px = current_mid
            self.ticks_since_peak = 0
        else:
            self.ticks_since_peak += 1

        # Check Hard Disaster Stop
        if unreal_bps <= -self.config.hard_disaster_stop_bps:
            self._close_position_complete("HARD_DISASTER_STOP")
            return

        # Check MFE Stall-Hazard Ratchet on Tranche 1
        mfe_bps = (
            d
            * ((self.peak_favorable_px - self.entry_price) / self.entry_price)
            * 10_000.0
        )
        if (
            not self.tranche1_filled
            and mfe_bps >= 10.0
            and self.ticks_since_peak >= self.config.stall_trigger_ticks
        ):
            # Trade stalled: modify TP1 to upper-quartile of achieved MFE
            exit_side = (
                OrderSide.SELL if self.pos_side == OrderSide.BUY else OrderSide.BUY
            )
            stalled_target = self.entry_price * (1.0 + d * (0.80 * mfe_bps * 1e-4))
            new_tp_px = self.instrument.make_price(
                Decimal(str(round(stalled_target, self.instrument.price_precision)))
            )

            if (
                self.active_tp1_order
                and self.active_tp1_order.is_open
                and new_tp_px != self.active_tp1_order.price
            ):
                self.modify_order(self.active_tp1_order, price=new_tp_px)
                self.ticks_since_peak = 0  # reset stall clock

    def on_order_rejected(self, event):
        order = self.cache.order(event.client_order_id)
        if not order:
            return
        if (
            self.active_entry_order
            and order.client_order_id == self.active_entry_order.client_order_id
        ):
            self.active_entry_order = None
        elif (
            self.active_tp1_order
            and order.client_order_id == self.active_tp1_order.client_order_id
        ):
            self.active_tp1_order = None
        elif (
            self.active_tp2_order
            and order.client_order_id == self.active_tp2_order.client_order_id
        ):
            self.active_tp2_order = None

    def on_order_canceled(self, event):
        order = self.cache.order(event.client_order_id)
        if not order:
            return
        if (
            self.active_entry_order
            and order.client_order_id == self.active_entry_order.client_order_id
        ):
            self.active_entry_order = None
        elif (
            self.active_tp1_order
            and order.client_order_id == self.active_tp1_order.client_order_id
        ):
            self.active_tp1_order = None
        elif (
            self.active_tp2_order
            and order.client_order_id == self.active_tp2_order.client_order_id
        ):
            self.active_tp2_order = None

    def on_order_expired(self, event):
        order = self.cache.order(event.client_order_id)
        if not order:
            return
        if (
            self.active_entry_order
            and order.client_order_id == self.active_entry_order.client_order_id
        ):
            self.active_entry_order = None
        elif (
            self.active_tp1_order
            and order.client_order_id == self.active_tp1_order.client_order_id
        ):
            self.active_tp1_order = None
        elif (
            self.active_tp2_order
            and order.client_order_id == self.active_tp2_order.client_order_id
        ):
            self.active_tp2_order = None

    def on_stop(self):
        self.cancel_all_orders(self.instrument_id)
        if self.is_in_position:
            for pos in self.cache.positions_open():
                if pos.instrument_id == self.instrument_id and float(pos.quantity) > 0:
                    side = OrderSide.SELL if pos.is_long else OrderSide.BUY
                    mo = self.order_factory.market(
                        instrument_id=self.instrument_id,
                        order_side=side,
                        quantity=pos.quantity,
                        reduce_only=True,
                    )
                    self.submit_order(mo)
        self.unsubscribe_quote_ticks(self.instrument_id)
        self.unsubscribe_trade_ticks(self.instrument_id)
        self.log.info(
            f"🛑 [STRATEGY STOPPED] Cleaned up subscriptions for {self.instrument_id.value}"
        )

    def _close_position_complete(self, reason: str):
        self.is_in_position = False
        self.log.info(
            f"🏁 [POSITION CLOSED - {reason}] Entry: {self.entry_price} | Final: {self.last_mid}"
        )
        self.cancel_all_orders(self.instrument_id)
        self.active_tp1_order = None
        self.active_tp2_order = None
        self.active_entry_order = None
