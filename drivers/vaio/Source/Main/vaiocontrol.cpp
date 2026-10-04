// SPDX-License-Identifier: MIT
// Confluence VAIO: control device, attach IOCTL and the link the stream timer
// pumps through.
//
// Lifetime of the engine's memory: the engine's attach IOCTL stays pending.
// Its output buffer (the shared region) is locked by the I/O manager for as
// long as the IRP is not completed. Every path that completes it (cancel on
// engine exit, cleanup on handle close, device removal) first clears g_Link
// under g_Lock, and the stream only touches the region while holding g_Lock,
// so nothing writes to the region once its IRP is completed.

#include <ntddk.h>
#include <wdmsec.h>
#include "vaiocontrol.h"
#include "vaio_pump.h"

#define VAIO_POOL_TAG 'oiaV'

// {5F8EC78C-CACC-431D-AB52-D96F6D3C1071}
static const GUID GUID_CONFLUENCE_VAIO_CONTROL =
    { 0x5f8ec78c, 0xcacc, 0x431d, { 0xab, 0x52, 0xd9, 0x6f, 0x6d, 0x3c, 0x10, 0x71 } };

static DRIVER_CANCEL   CancelAttach;
static DRIVER_DISPATCH VaioDispatch;

static PDRIVER_DISPATCH g_PcDispatch[IRP_MJ_MAXIMUM_FUNCTION + 1];
static PDEVICE_OBJECT   g_ControlDevice = NULL;

// The control device carries this tagged extension, so its IRPs are recognised
// even after it was deleted while a handle was still open (they must never
// reach portcls, whose dispatch expects its own device objects).
#define VAIO_CONTROL_MAGIC 0x4F494156434F4E54ULL   /* "TNOCVAIO" */
typedef struct VAIO_CONTROL_EXTENSION
{
    ULONGLONG     Magic;
    volatile LONG Deleted;   // removed; only CLEANUP and CLOSE still succeed
} VAIO_CONTROL_EXTENSION;

static VAIO_CONTROL_EXTENSION* ControlExtension(PDEVICE_OBJECT DeviceObject)
{
    // Only two kinds of device object reach this driver's dispatch: portcls's
    // (whose extension is far larger than ours, so reading its first bytes is
    // safe) and our control device. Only ours holds the tag.
    if (DeviceObject->DeviceType != FILE_DEVICE_SOUND || DeviceObject->DeviceExtension == NULL)
    {
        return NULL;
    }
    VAIO_CONTROL_EXTENSION* ext = (VAIO_CONTROL_EXTENSION*)DeviceObject->DeviceExtension;
    return ext->Magic == VAIO_CONTROL_MAGIC ? ext : NULL;
}
static KSPIN_LOCK       g_Lock;
static vaio::Link       g_Link = {};
static PIRP             g_AttachIrp = NULL;
static BOOLEAN          g_Streaming = FALSE;

static LONGLONG NowMs()
{
    return (LONGLONG)(KeQueryInterruptTime() / 10000);
}

static NTSTATUS Complete(PIRP Irp, NTSTATUS Status)
{
    Irp->IoStatus.Status = Status;
    Irp->IoStatus.Information = 0;
    IoCompleteRequest(Irp, IO_NO_INCREMENT);
    return Status;
}

// Takes the pending attach IRP away from the link. Returns it only if the
// caller now owns its completion (its cancel routine will not run).
static PIRP TakeAttachIrp()
{
    KIRQL irql;
    KeAcquireSpinLock(&g_Lock, &irql);
    PIRP irp = g_AttachIrp;
    g_AttachIrp = NULL;
    vaio::detach(g_Link);
    KeReleaseSpinLock(&g_Lock, irql);
    if (irp != NULL && IoSetCancelRoutine(irp, NULL) == NULL)
    {
        irp = NULL; // the cancel routine is running and completes it
    }
    return irp;
}

static void Detach()
{
    PIRP irp = TakeAttachIrp();
    if (irp != NULL)
    {
        Complete(irp, STATUS_SUCCESS);
    }
}

_Use_decl_annotations_
static VOID CancelAttach(PDEVICE_OBJECT DeviceObject, PIRP Irp)
{
    UNREFERENCED_PARAMETER(DeviceObject);
    IoReleaseCancelSpinLock(Irp->CancelIrql);
    KIRQL irql;
    KeAcquireSpinLock(&g_Lock, &irql);
    if (g_AttachIrp == Irp)
    {
        g_AttachIrp = NULL;
        vaio::detach(g_Link);
    }
    KeReleaseSpinLock(&g_Lock, irql);
    Complete(Irp, STATUS_CANCELLED);
}

static NTSTATUS Attach(PIRP Irp)
{
    PIO_STACK_LOCATION sl = IoGetCurrentIrpStackLocation(Irp);
    if (sl->Parameters.DeviceIoControl.IoControlCode != CONFLUENCE_VAIO_IOCTL_ATTACH)
    {
        return Complete(Irp, STATUS_INVALID_DEVICE_REQUEST);
    }
    if (Irp->MdlAddress == NULL)
    {
        return Complete(Irp, STATUS_INVALID_PARAMETER);
    }
    ULONG length = sl->Parameters.DeviceIoControl.OutputBufferLength;
    PVOID region = MmGetSystemAddressForMdlSafe(Irp->MdlAddress, NormalPagePriority | MdlMappingNoExecute);
    if (region == NULL)
    {
        return Complete(Irp, STATUS_INSUFFICIENT_RESOURCES);
    }

    KIRQL irql;
    KeAcquireSpinLock(&g_Lock, &irql);
    if (g_AttachIrp != NULL)
    {
        KeReleaseSpinLock(&g_Lock, irql);
        return Complete(Irp, STATUS_DEVICE_BUSY);
    }
    vaio::Link link = {};
    if (!vaio::attach(link, region, length, NowMs()))
    {
        KeReleaseSpinLock(&g_Lock, irql);
        return Complete(Irp, STATUS_INVALID_PARAMETER);
    }
    IoMarkIrpPending(Irp);
    IoSetCancelRoutine(Irp, CancelAttach);
    if (Irp->Cancel && IoSetCancelRoutine(Irp, NULL) != NULL)
    {
        // Cancelled before we could queue it, and we still own it.
        KeReleaseSpinLock(&g_Lock, irql);
        Complete(Irp, STATUS_CANCELLED);
        return STATUS_PENDING;
    }
    // Queued. If a cancel raced in, its routine finds g_AttachIrp == Irp and completes it.
    g_AttachIrp = Irp;
    g_Link = link;
    vaio::set_streaming(g_Link, g_Streaming != FALSE);
    KeReleaseSpinLock(&g_Lock, irql);
    return STATUS_PENDING;
}

_Use_decl_annotations_
static NTSTATUS VaioDispatch(PDEVICE_OBJECT DeviceObject, PIRP Irp)
{
    UCHAR major = IoGetCurrentIrpStackLocation(Irp)->MajorFunction;
    VAIO_CONTROL_EXTENSION* ext = ControlExtension(DeviceObject);
    if (ext == NULL)
    {
        return g_PcDispatch[major](DeviceObject, Irp);
    }
    BOOLEAN deleted = ext->Deleted != 0;
    switch (major)
    {
    case IRP_MJ_CREATE:
        return Complete(Irp, deleted ? STATUS_DELETE_PENDING : STATUS_SUCCESS);
    case IRP_MJ_CLOSE:
        return Complete(Irp, STATUS_SUCCESS);
    case IRP_MJ_CLEANUP:
        // The engine closed its handle (or exited): let go of its memory. A
        // deleted control device already let go (VaioControlDelete), and the
        // link may by now belong to a new one.
        if (!deleted)
        {
            Detach();
        }
        return Complete(Irp, STATUS_SUCCESS);
    case IRP_MJ_DEVICE_CONTROL:
        return deleted ? Complete(Irp, STATUS_DELETE_PENDING) : Attach(Irp);
    default:
        return Complete(Irp, STATUS_INVALID_DEVICE_REQUEST);
    }
}

_Use_decl_annotations_
void VaioInstallDispatch(PDRIVER_OBJECT DriverObject)
{
    KeInitializeSpinLock(&g_Lock);
    // Wrapping portcls's dispatch table is the point of this function, so the
    // "should not access MajorFunction" warning is expected here, and a catch-all
    // wrapper cannot carry a _Dispatch_type_ for every major function (each IRP
    // still reaches the routine portcls installed).
#pragma warning(push)
#pragma warning(disable: 28175 28168 28169)
    for (ULONG i = 0; i <= IRP_MJ_MAXIMUM_FUNCTION; ++i)
    {
        g_PcDispatch[i] = DriverObject->MajorFunction[i];
        DriverObject->MajorFunction[i] = VaioDispatch;
    }
#pragma warning(pop)
}

_Use_decl_annotations_
NTSTATUS VaioControlCreate(PDRIVER_OBJECT DriverObject)
{
    if (g_ControlDevice != NULL)
    {
        return STATUS_SUCCESS;
    }
    UNICODE_STRING name = RTL_CONSTANT_STRING(CONFLUENCE_VAIO_DEVICE_NAME);
    UNICODE_STRING link = RTL_CONSTANT_STRING(CONFLUENCE_VAIO_SYMLINK);
    // SYSTEM and Administrators: all; interactive users: read/write.
    UNICODE_STRING sddl = RTL_CONSTANT_STRING(L"D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)");
    PDEVICE_OBJECT device = NULL;
    NTSTATUS status = IoCreateDeviceSecure(DriverObject, sizeof(VAIO_CONTROL_EXTENSION), &name, FILE_DEVICE_SOUND,
                                           FILE_DEVICE_SECURE_OPEN,
                                           TRUE /* exclusive: one engine */, &sddl, &GUID_CONFLUENCE_VAIO_CONTROL,
                                           &device);
    if (!NT_SUCCESS(status))
    {
        return status;
    }
    status = IoCreateSymbolicLink(&link, &name);
    if (!NT_SUCCESS(status))
    {
        IoDeleteDevice(device);
        return status;
    }
    VAIO_CONTROL_EXTENSION* ext = (VAIO_CONTROL_EXTENSION*)device->DeviceExtension;
    ext->Magic = VAIO_CONTROL_MAGIC;
    ext->Deleted = 0;
    g_ControlDevice = device;
    device->Flags &= ~DO_DEVICE_INITIALIZING;   // only now can it be opened
    return STATUS_SUCCESS;
}

void VaioControlDelete()
{
    Detach();
    if (g_ControlDevice != NULL)
    {
        UNICODE_STRING link = RTL_CONSTANT_STRING(CONFLUENCE_VAIO_SYMLINK);
        IoDeleteSymbolicLink(&link);
        PDEVICE_OBJECT device = g_ControlDevice;
        g_ControlDevice = NULL;
        // A handle may stay open past the delete: its IRPs keep coming to us.
        InterlockedExchange(&((VAIO_CONTROL_EXTENSION*)device->DeviceExtension)->Deleted, 1);
        IoDeleteDevice(device);
    }
}

_Use_decl_annotations_
BOOLEAN VaioAdvance(ULONG timeBytes, ULONG limitBytes, const UCHAR* src, ULONG srcSize, ULONG srcOffset,
                    ULONG* advanced)
{
    *advanced = 0;
    BOOLEAN driven = FALSE;
    KIRQL irql;
    KeAcquireSpinLock(&g_Lock, &irql);
    if (g_Link.header != NULL && src != NULL && srcSize != 0)
    {
        if (vaio::engine_alive(g_Link, NowMs()))
        {
            ULONG limit = limitBytes < srcSize ? limitBytes : srcSize;
            ULONG paced = vaio::pace(timeBytes);
            limit = paced < limit ? paced : limit;
            ULONG bytes = vaio::plan(g_Link, limit);
            vaio::copy(g_Link, src, srcSize, srcOffset, bytes);
            *advanced = bytes;
            driven = TRUE;
        }
        else
        {
            vaio::note_free_run(g_Link, timeBytes);
        }
    }
    KeReleaseSpinLock(&g_Lock, irql);
    return driven;
}

BOOLEAN VaioIsAttached()
{
    KIRQL irql;
    KeAcquireSpinLock(&g_Lock, &irql);
    BOOLEAN attached = g_Link.header != NULL;
    KeReleaseSpinLock(&g_Lock, irql);
    return attached;
}

void VaioSetStreaming(BOOLEAN on)
{
    KIRQL irql;
    KeAcquireSpinLock(&g_Lock, &irql);
    g_Streaming = on;
    vaio::set_streaming(g_Link, on != FALSE);
    KeReleaseSpinLock(&g_Lock, irql);
}
