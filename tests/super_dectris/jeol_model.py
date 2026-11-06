"""A column as the simulated microscope does not model it: commands take time, and the stage speeds up and slows down.

Known: any command or reading takes about 3 ms at best, over
the network to the column's embedded system and back. Everything else is assumed, nothing
is measured: the fields are there to try a range of what a real stage may do.
"""

from __future__ import annotations

import random
import time
from dataclasses import dataclass

from instamatic.microscope.interface.simu_microscope import SimuMicroscope
from instamatic.microscope.utils import StagePositionTuple


@dataclass
class ColumnModel:
    trip: float = 0.003  # seconds: a command or reading at best, there and back
    trip_extra: float = 0.001  # seconds: the mean of what comes on top, exponential
    stall_chance: float = 0.002  # of a trip taking `stall` longer
    stall: float = 0.03  # seconds
    ramp: float = 0.3  # seconds to reach the rotation speed, and to lose it
    start_delay: float = 0.02  # seconds from a command to the first movement
    noise: float = 0.005  # degrees, the most a reading is off, even
    digits: int = 2  # decimals of a reading
    start_angle: float = 0.0  # degrees
    seed: int = 1


class Move:
    """From `start` towards `target` from time `t0`: run-up, steady speed, slowing down. Runs at `speed` deg/s."""

    def __init__(self, t0: float, start: float, target: float, speed: float, ramp: float):
        self.t0, self.start = t0, start
        self.distance = abs(target - start)
        self.sign = 1 if target >= start else -1
        self.speed = speed
        self.ramp = min(ramp, self.distance / speed)  # a short move does not reach its speed for long
        self.t_slow = self.distance / speed  # after t0, as the run-up costs half its own length

    @property
    def t_end(self) -> float:
        return self.t0 + self.t_slow + self.ramp

    def angle(self, t: float) -> float:
        dt = t - self.t0
        if dt <= 0:
            done = 0.0
        elif self.ramp == 0:
            done = min(dt * self.speed, self.distance)
        elif dt < self.ramp:
            done = self.speed * dt * dt / (2 * self.ramp)
        elif dt < self.t_slow:
            done = self.speed * (dt - self.ramp / 2)
        elif dt < self.t_slow + self.ramp:
            left = self.t_slow + self.ramp - dt
            done = self.distance - self.speed * left * left / (2 * self.ramp)
        else:
            done = self.distance
        return self.start + self.sign * done

    def speed_at(self, t: float) -> float:
        dt = t - self.t0
        if dt <= 0 or dt >= self.t_slow + self.ramp:
            return 0.0
        if self.ramp and dt < self.ramp:
            return self.speed * dt / self.ramp
        if dt < self.t_slow:
            return self.speed
        return self.speed * (self.t_slow + self.ramp - dt) / self.ramp if self.ramp else self.speed


class JeolLikeMicroscope(SimuMicroscope):
    """`true_angle(t)` is where the stage was; `focus_commands` when each focus reached the lens."""

    def __init__(self, model: ColumnModel | None = None, **kwargs):
        super().__init__(**kwargs)
        self.model = model or ColumnModel()
        self.random = random.Random(self.model.seed)
        self.moves = [Move(0.0, self.model.start_angle, self.model.start_angle, 1.0, 0.0)]
        self.focus_commands = []
        self.reads = []  # how long each reading of the stage took

    def _half_trip(self) -> None:
        m = self.model
        wait = m.trip / 2 + self.random.expovariate(2 / m.trip_extra) if m.trip_extra else m.trip / 2
        if self.random.random() < m.stall_chance:
            wait += m.stall / 2
        time.sleep(wait)

    def true_angle(self, t: float) -> float:
        for move in reversed(self.moves):
            if t >= move.t0:
                return move.angle(t)
        return self.moves[0].start

    def getStagePosition(self) -> StagePositionTuple:
        t = time.perf_counter()
        self._half_trip()
        reading = self.true_angle(time.perf_counter()) + self.random.uniform(-self.model.noise, self.model.noise)
        self._half_trip()
        self.reads.append(time.perf_counter() - t)
        x, y, z, _, b = super().getStagePosition()
        return StagePositionTuple(x, y, z, round(reading, self.model.digits), b)

    def isStageMoving(self) -> bool:
        self._half_trip()
        moving = time.perf_counter() < self.moves[-1].t_end
        self._half_trip()
        return moving

    def setStageA(self, value: float, wait: bool = True) -> None:
        self._half_trip()
        now = time.perf_counter()
        speed = self._stage_dict['a']['speed']
        self.moves.append(Move(now + self.model.start_delay, self.true_angle(now), value, speed, self.model.ramp))
        self._half_trip()
        if wait:
            self.waitForStage()

    def stopStage(self) -> None:
        self._half_trip()
        now = time.perf_counter()
        move = self.moves[-1]
        speed = move.speed_at(now)
        if speed:  # slows down from here
            ramp = self.model.ramp
            target = self.true_angle(now) + move.sign * speed * ramp / 2
            stop = Move(now, self.true_angle(now), target, speed, 0.0)
            if ramp:
                stop.ramp, stop.t_slow = ramp, 0.0
            self.moves.append(stop)
        self._half_trip()

    def setDiffFocus(self, value: int, confirm_mode: bool = True) -> None:
        self._half_trip()
        self.focus_commands.append((time.perf_counter(), value))
        super().setDiffFocus(value, confirm_mode=False)
        self._half_trip()
