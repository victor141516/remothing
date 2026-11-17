// Control interface of remothing's virtual display driver, SudoVDA's with
// its own hardware id and interface GUID so it lives next to an installed
// SudoVDA (Apollo) without either seeing the other's devices.
// crates/platform-windows/src/vdd.rs mirrors these definitions.
#pragma once

#include <windows.h>

#ifdef __cplusplus
namespace REMOTHING_VDD
{
#endif

#define IOCTL_ADD_VIRTUAL_DISPLAY CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS)
#define IOCTL_REMOVE_VIRTUAL_DISPLAY CTL_CODE(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_ANY_ACCESS)
#define IOCTL_SET_RENDER_ADAPTER CTL_CODE(FILE_DEVICE_UNKNOWN, 0x802, METHOD_BUFFERED, FILE_ANY_ACCESS)
#define IOCTL_GET_WATCHDOG CTL_CODE(FILE_DEVICE_UNKNOWN, 0x803, METHOD_BUFFERED, FILE_ANY_ACCESS)
#define IOCTL_DRIVER_PING CTL_CODE(FILE_DEVICE_UNKNOWN, 0x888, METHOD_BUFFERED, FILE_ANY_ACCESS)
#define IOCTL_GET_PROTOCOL_VERSION CTL_CODE(FILE_DEVICE_UNKNOWN, 0x8FF, METHOD_BUFFERED, FILE_ANY_ACCESS)

typedef struct _SUVDA_PROTOCAL_VERSION {
	uint8_t Major;
	uint8_t Minor;
	uint8_t Incremental;
	bool TestBuild;
} SUVDA_PROTOCAL_VERSION, * PSUVDA_PROTOCAL_VERSION;

// Please update the version after ioctl changed
static const SUVDA_PROTOCAL_VERSION VDAProtocolVersion = { 0, 2, 1, true };

static const char* SUVDA_HARDWARE_ID = "root\\remothing\\vdd";

// The Display device class; do not change.
// {4d36e968-e325-11ce-bfc1-08002be10318}
static const GUID SUVDA_CLASS_GUID = { 0x4d36e968, 0xe325, 0x11ce, { 0xbf, 0xc1, 0x08, 0x00, 0x2b, 0xe1, 0x03, 0x18 } };
// {c66e256f-0f7e-4d04-b1d5-9136bed1fbf0}
static const GUID SUVDA_INTERFACE_GUID = { 0xc66e256f, 0x0f7e, 0x4d04, { 0xb1, 0xd5, 0x91, 0x36, 0xbe, 0xd1, 0xfb, 0xf0 } };

typedef struct _VIRTUAL_DISPLAY_ADD_PARAMS {
	UINT Width;
	UINT Height;
	UINT RefreshRate;
	GUID MonitorGuid;
	CHAR DeviceName[14];
	CHAR SerialNumber[14];
} VIRTUAL_DISPLAY_ADD_PARAMS, * PVIRTUAL_DISPLAY_ADD_PARAMS;

typedef struct _VIRTUAL_DISPLAY_REMOVE_PARAMS {
	GUID MonitorGuid;
} VIRTUAL_DISPLAY_REMOVE_PARAMS, * PVIRTUAL_DISPLAY_REMOVE_PARAMS;

typedef struct _VIRTUAL_DISPLAY_ADD_OUT {
	LUID AdapterLuid;
	UINT TargetId;
} VIRTUAL_DISPLAY_ADD_OUT, * PVIRTUAL_DISPLAY_ADD_OUT;

typedef struct _VIRTUAL_DISPLAY_SET_RENDER_ADAPTER_PARAMS {
	LUID AdapterLuid;
} VIRTUAL_DISPLAY_SET_RENDER_ADAPTER_PARAMS, * PVIRTUAL_DISPLAY_SET_RENDER_ADAPTER_PARAMS;

typedef struct _VIRTUAL_DISPLAY_GET_WATCHDOG_OUT {
	UINT Timeout;
	UINT Countdown;
} VIRTUAL_DISPLAY_GET_WATCHDOG_OUT, * PVIRTUAL_DISPLAY_GET_WATCHDOG_OUT;

typedef struct _VIRTUAL_DISPLAY_GET_PROTOCOL_VERSION_OUT {
	SUVDA_PROTOCAL_VERSION Version;
} VIRTUAL_DISPLAY_GET_PROTOCOL_VERSION_OUT, * PVIRTUAL_DISPLAY_GET_PROTOCOL_VERSION_OUT;

#ifdef __cplusplus
} // namespace REMOTHING_VDD
#endif