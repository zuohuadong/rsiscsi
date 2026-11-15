#ifndef ISCSI_VIRTUAL_DISK_H
#define ISCSI_VIRTUAL_DISK_H

#include <IOKit/IOLib.h>
#include <IOKit/IOLocks.h>
#include <IOKit/IOBufferMemoryDescriptor.h>
#include <IOKit/IOSharedDataQueue.h>
#include <IOKit/storage/IOBlockStorageDevice.h>
#include <IOKit/storage/IOMedia.h>
#include <IOKit/storage/IOStorage.h>

#include "ISCSIProtocol.h"

class ISCSIVirtualDiskUserClient;

/*
 * One in-flight storage request. A request is split into chunks so that a
 * single large read never has to wait for one huge arena allocation; every
 * chunk is queued separately and the storage completion fires once all of
 * them reported back.
 */
struct ISCSIPendingIO {
	bool                inUse;
	uint32_t            token;
	uint32_t            generation;
	uint32_t            chunkBytes;
	uint32_t            chunkCount;
	uint32_t            chunksDone;
	uint32_t            bytesDone;
	IOReturn            status;
	IOMemoryDescriptor *buffer;
	UInt64              bufferOffset;
	UInt32              chunkLength[kISCSIVDChunksPerIO];
	UInt32              chunkArena[kISCSIVDChunksPerIO];
	bool                chunkDone[kISCSIVDChunksPerIO];
	IOStorageCompletion completion;
};

class ISCSIVirtualDisk : public IOBlockStorageDevice {
	OSDeclareDefaultStructors(ISCSIVirtualDisk)
	friend class ISCSIVirtualDiskUserClient;

public:
	bool init(OSDictionary *dictionary = 0) APPLE_KEXT_OVERRIDE;
	bool start(IOService *provider) APPLE_KEXT_OVERRIDE;
	void stop(IOService *provider) APPLE_KEXT_OVERRIDE;
	void free() APPLE_KEXT_OVERRIDE;

	IOReturn newUserClient(task_t owningTask, void *securityID, UInt32 type,
	    OSDictionary *properties,
	    IOUserClient **handler) APPLE_KEXT_OVERRIDE;

	/* IOBlockStorageDevice */
	IOReturn doEjectMedia(void) APPLE_KEXT_OVERRIDE;
	IOReturn doFormatMedia(UInt64 byteCapacity) APPLE_KEXT_OVERRIDE;
	UInt32   doGetFormatCapacities(UInt64 *capacities,
	    UInt32 capacitiesMaxCount) const APPLE_KEXT_OVERRIDE;
	IOReturn doSynchronizeCache(void) APPLE_KEXT_OVERRIDE;
	char *   getVendorString(void) APPLE_KEXT_OVERRIDE;
	char *   getProductString(void) APPLE_KEXT_OVERRIDE;
	char *   getRevisionString(void) APPLE_KEXT_OVERRIDE;
	char *   getAdditionalDeviceInfoString(void) APPLE_KEXT_OVERRIDE;
	IOReturn reportBlockSize(UInt64 *blockSize) APPLE_KEXT_OVERRIDE;
	IOReturn reportEjectability(bool *isEjectable) APPLE_KEXT_OVERRIDE;
	IOReturn reportMaxValidBlock(UInt64 *maxBlock) APPLE_KEXT_OVERRIDE;
	IOReturn reportMediaState(bool *mediaPresent,
	    bool *changedState = 0) APPLE_KEXT_OVERRIDE;
	IOReturn reportRemovability(bool *isRemovable) APPLE_KEXT_OVERRIDE;
	IOReturn reportWriteProtection(bool *isWriteProtected) APPLE_KEXT_OVERRIDE;
	IOReturn doAsyncReadWrite(IOMemoryDescriptor *buffer, UInt64 block,
	    UInt64 nblks, IOStorageAttributes *attributes,
	    IOStorageCompletion *completion) APPLE_KEXT_OVERRIDE;
	IOReturn doUnmap(IOBlockStorageDeviceExtent *extents, UInt32 extentsCount,
	    IOStorageUnmapOptions options = 0) APPLE_KEXT_OVERRIDE;
	IOReturn doSynchronize(UInt64 block, UInt64 nblks,
	    IOStorageSynchronizeOptions options = 0) APPLE_KEXT_OVERRIDE;
	IOReturn requestIdle(void) APPLE_KEXT_OVERRIDE;
	IOReturn getWriteCacheState(bool *enabled) APPLE_KEXT_OVERRIDE;
	IOReturn setWriteCacheState(bool enabled) APPLE_KEXT_OVERRIDE;

private:
	/* User client lifecycle. */
	bool     attachUserClient(ISCSIVirtualDiskUserClient *client);
	void     detachUserClient(ISCSIVirtualDiskUserClient *client);
	IOReturn setGeometry(UInt32 blockSize, UInt64 blockCount);
	IOReturn setOnline(bool online);
	IOReturn completeChunk(UInt32 token, UInt32 chunk, UInt32 status,
	    UInt32 bytes);
	IOReturn getConfig(UInt64 *output, UInt32 *outputCount);
	IOMemoryDescriptor *copyMemoryForType(UInt32 type);

	/* Arena allocator, all *_Locked calls require fLock. */
	void *   arenaBaseLocked(void) const;
	UInt32   arenaAllocLocked(UInt32 bytes);
	void     arenaFreeLocked(UInt32 offset, UInt32 bytes);
	ISCSIPendingIO *pendingForTokenLocked(UInt32 token);
	void     failAllPending(void);

	static const UInt32 kInvalidArenaOffset = 0xffffffffu;

	IOLock *                    fLock;
	IOBufferMemoryDescriptor *  fArena;
	IOSharedDataQueue *         fRequests;
	ISCSIVirtualDiskUserClient *fClient;
	ISCSIPendingIO              fPending[kISCSIVDPendingIO];
	bool                        fOnline;
	bool                        fMediaChanged;
	UInt32                      fBlockSize;
	UInt64                      fBlockCount;
	UInt32                      fArenaBytes;
	UInt32                      fPageCount;
	uint8_t *                   fPageMap;
	UInt32                      fGeneration;
};

#endif /* ISCSI_VIRTUAL_DISK_H */
