#include "ISCSIVirtualDisk.h"
#include "ISCSIVirtualDiskUserClient.h"

#define super IOUserClient
OSDefineMetaClassAndStructors(ISCSIVirtualDiskUserClient, IOUserClient)

bool ISCSIVirtualDiskUserClient::initWithTask(task_t owningTask,
    void *securityToken, UInt32 type, OSDictionary *properties)
{
	if (!super::initWithTask(owningTask, securityToken, type, properties)) {
		return false;
	}

	fProvider = NULL;
	if (clientHasPrivilege(securityToken,
	        kIOClientPrivilegeAdministrator) != kIOReturnSuccess) {
		IOLog("ISCSIVirtualDiskUserClient: root is required\n");
		return false;
	}
	return true;
}

bool ISCSIVirtualDiskUserClient::start(IOService *provider)
{
	if (!super::start(provider)) {
		return false;
	}

	fProvider = OSDynamicCast(ISCSIVirtualDisk, provider);
	if (!fProvider || !fProvider->attachUserClient(this)) {
		IOLog("ISCSIVirtualDiskUserClient: device already in use\n");
		fProvider = NULL;
		return false;
	}
	return true;
}

void ISCSIVirtualDiskUserClient::stop(IOService *provider)
{
	detachProvider();
	super::stop(provider);
}

IOReturn ISCSIVirtualDiskUserClient::detachProvider(void)
{
	ISCSIVirtualDisk *provider = fProvider;
	if (!provider) {
		return kIOReturnSuccess;
	}
	fProvider = NULL;
	provider->detachUserClient(this);
	return kIOReturnSuccess;
}

IOReturn ISCSIVirtualDiskUserClient::clientClose(void)
{
	detachProvider();
	return super::clientClose();
}

IOReturn ISCSIVirtualDiskUserClient::clientDied(void)
{
	detachProvider();
	return super::clientDied();
}

IOReturn ISCSIVirtualDiskUserClient::externalMethod(uint32_t selector,
    IOExternalMethodArguments *arguments, IOExternalMethodDispatch *dispatch,
    OSObject *target, void *reference)
{
	(void)dispatch;
	(void)target;
	(void)reference;

	if (!arguments || selector >= kISCSIVDMethodCount) {
		return kIOReturnUnsupported;
	}
	if (!fProvider) {
		return kIOReturnNotReady;
	}

	switch (selector) {
	case kISCSIVDMethodSetGeometry:
		if (arguments->scalarInputCount < 2) {
			return kIOReturnBadArgument;
		}
		return fProvider->setGeometry((UInt32)arguments->scalarInput[0],
		    (UInt64)arguments->scalarInput[1]);

	case kISCSIVDMethodCompleteIO:
		if (arguments->scalarInputCount < 4) {
			return kIOReturnBadArgument;
		}
		return fProvider->completeChunk((UInt32)arguments->scalarInput[0],
		    (UInt32)arguments->scalarInput[1],
		    (UInt32)arguments->scalarInput[2],
		    (UInt32)arguments->scalarInput[3]);

	case kISCSIVDMethodSetOnline:
		if (arguments->scalarInputCount < 1) {
			return kIOReturnBadArgument;
		}
		return fProvider->setOnline(arguments->scalarInput[0] != 0);

	case kISCSIVDMethodGetConfig: {
		if (arguments->scalarOutputCount < 4 || !arguments->scalarOutput) {
			return kIOReturnBadArgument;
		}
		UInt32 count = 4;
		IOReturn result = fProvider->getConfig(arguments->scalarOutput, &count);
		arguments->scalarOutputCount = count;
		return result;
	}

	default:
		break;
	}
	return kIOReturnUnsupported;
}

IOReturn ISCSIVirtualDiskUserClient::clientMemoryForType(UInt32 type,
    IOOptionBits *options, IOMemoryDescriptor **memory)
{
	if (!memory) {
		return kIOReturnBadArgument;
	}
	if (options) {
		*options = 0;
	}
	if (!fProvider) {
		return kIOReturnNotReady;
	}

	IOMemoryDescriptor *descriptor = fProvider->copyMemoryForType(type);
	if (!descriptor) {
		return kIOReturnUnsupported;
	}
	*memory = descriptor;
	return kIOReturnSuccess;
}
