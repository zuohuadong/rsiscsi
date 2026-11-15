#ifndef ISCSI_VIRTUAL_DISK_USER_CLIENT_H
#define ISCSI_VIRTUAL_DISK_USER_CLIENT_H

#include <IOKit/IOUserClient.h>

class ISCSIVirtualDisk;

/*
 * Root-only connection to ISCSIVirtualDisk. The daemon maps the shared arena
 * and the request queue, then reports completions through external methods.
 */
class ISCSIVirtualDiskUserClient : public IOUserClient {
	OSDeclareDefaultStructors(ISCSIVirtualDiskUserClient)

public:
	bool initWithTask(task_t owningTask, void *securityToken, UInt32 type,
	    OSDictionary *properties) APPLE_KEXT_OVERRIDE;
	bool start(IOService *provider) APPLE_KEXT_OVERRIDE;
	void stop(IOService *provider) APPLE_KEXT_OVERRIDE;
	IOReturn clientClose(void) APPLE_KEXT_OVERRIDE;
	IOReturn clientDied(void) APPLE_KEXT_OVERRIDE;
	IOReturn externalMethod(uint32_t selector,
	    IOExternalMethodArguments *arguments,
	    IOExternalMethodDispatch *dispatch, OSObject *target,
	    void *reference) APPLE_KEXT_OVERRIDE;
	IOReturn clientMemoryForType(UInt32 type, IOOptionBits *options,
	    IOMemoryDescriptor **memory) APPLE_KEXT_OVERRIDE;

private:
	IOReturn detachProvider(void);

	ISCSIVirtualDisk *fProvider;
};

#endif /* ISCSI_VIRTUAL_DISK_USER_CLIENT_H */
