// SPDX-License-Identifier: MIT
// Confluence VAIO: the control device the engine attaches through.
#pragma once

#include <ntddk.h>

// Wraps every major function: IRPs for the control device are ours, all
// others go to what portcls installed. Call after PcInitializeAdapterDriver
// and after any other MajorFunction override.
void VaioInstallDispatch(_In_ PDRIVER_OBJECT DriverObject);

// \Device\ConfluenceVaio + \DosDevices\Global\ConfluenceVaio, exclusive.
NTSTATUS VaioControlCreate(_In_ PDRIVER_OBJECT DriverObject);

// Detaches any engine and deletes the control device. Idempotent.
void VaioControlDelete();

// Stream side, callable at DISPATCH_LEVEL. When an attached engine is alive,
// copies up to limitBytes (at most what fills the ring to its target) from the
// cyclic buffer, stores the byte count in *advanced and returns TRUE: the
// stream must advance by exactly that. Otherwise returns FALSE (the stream
// free-runs on its own clock) and counts timeBytes as played without the engine.
BOOLEAN VaioAdvance(ULONG timeBytes, ULONG limitBytes, _In_reads_bytes_(srcSize) const UCHAR* src,
                    ULONG srcSize, ULONG srcOffset, _Out_ ULONG* advanced);

BOOLEAN VaioIsAttached();

// Whether an app stream runs (shown to the engine; no silence counted as xruns otherwise).
void VaioSetStreaming(BOOLEAN on);
