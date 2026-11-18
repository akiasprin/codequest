#pragma once
/// Emit an event independently of scheduling.
void emit(int);
struct Sink { virtual void write(int) = 0; virtual ~Sink() = default; };
struct Writer : Sink { void write(int) override; };
