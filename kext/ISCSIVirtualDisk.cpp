#include <kern/task.h>
#include <string.h>
#include <IOKit/storage/IOBlockStorageDriver.h>

#include "ISCSIVirtualDisk.h"
#include "ISCSIVirtualDiskUserClient.h"

#define super IOBlockStorageDevice
OSDefineMetaClassAndStructors(ISCSIVirtualDisk, IOBlockStorageDevice)

namespace {

/*
 * Copy payload into the caller's read buffer. createMappingInTask() works on
 * descriptors that are not prepared for DMA and does not depend on the
 * direction bits, unlike IOMemoryDescriptor::writeBytes().
 */
IOReturn copyIntoDescriptor(IOMemoryDescriptor *buffer, UInt64 offset,
    const void *source, UInt32 length)
{
	if (!buffer) {
		return kIOReturnBadArgument;
	}
	if (length == 0) {
		return kIOReturnSuccess;
	}

	IOMemoryMap *map = buffer->createMappingInTask(kernel_task, 0, kIOMapAnywhere);
	if (!map) {
		return kIOReturnNoMemory;
	}

	IOReturn result = kIOReturnSuccess;
	if (offset + (UInt64)length > (UInt64)map->getLength()) {
		result = kIOReturnBadArgument;
	} else {
		memcpy((uint8_t *)map->getAddress() + offset, source, length);
	}
	map->release();
	return result;
}

} // namespace

bool ISCSIVirtualDisk::init(OSDictionary *dictionary)
{
	if (!super::init(dictionary)) {
		return false;
	}

	fLock = NULL;
	fArena = NULL;
	fRequests = NULL;
	fClient = NULL;
	fOnline = false;
	fMediaChanged = true;
	fBlockSize = 0;
	fBlockCount = 0;
	fArenaBytes = 0;
	fPageCount = 0;
	fPageMap = NULL;
	fGeneration = 0;
	bzero(fPending, sizeof(fPending));
	return true;
}

bool ISCSIVirtualDisk::start(IOService *provider)
{
	if (!super::start(provider)) {
		return false;
	}

	fLock = IOLockAlloc();
	if (!fLock) {
		return false;
	}

	UInt32 arenaBytes = (UInt32)kISCSIVDDefaultArenaBytes;
	OSNumber *sizeProperty = OSDynamicCast(OSNumber, getProperty("ISCSIArenaBytes"));
	if (sizeProperty) {
		arenaBytes = sizeProperty->unsigned32BitValue();
	}
	if (arenaBytes < 4u * 1024u * 1024u) {
		arenaBytes = 4u * 1024u * 1024u;
	}
	if (arenaBytes > 512u * 1024u * 1024u) {
		arenaBytes = 512u * 1024u * 1024u;
	}
	arenaBytes &= ~(kISCSIVDPageGranule - 1u);

	fArena = IOBufferMemoryDescriptor::inTaskWithOptions(kernel_task,
	    kIOMemoryKernelUserShared | kIODirectionInOut, arenaBytes,
	    kISCSIVDPageGranule);
	if (!fArena) {
		IOLog("ISCSIVirtualDisk: arena allocation of %u bytes failed\n",
		    (unsigned)arenaBytes);
		return false;
	}
	fArenaBytes = arenaBytes;
	fPageCount = fArenaBytes / kISCSIVDPageGranule;

	UInt32 bitmapBytes = (fPageCount + 7) / 8;
	fPageMap = (uint8_t *)IOMalloc(bitmapBytes);
	if (!fPageMap) {
		return false;
	}
	bzero(fPageMap, bitmapBytes);

	fRequests = IOSharedDataQueue::withCapacity(kISCSIVDQueueEntries *
	    sizeof(iscsi_vd_request));
	if (!fRequests) {
		IOLog("ISCSIVirtualDisk: request queue allocation failed\n");
		return false;
	}

	setProperty("ISCSIVDABIVersion", (unsigned long long)kISCSIVDAbiVersion, 32);
	setProperty("ISCSIVDArenaBytes", (unsigned long long)fArenaBytes, 32);
	IOLog("ISCSIVirtualDisk: started, arena %u bytes, %u pages\n",
	    (unsigned)fArenaBytes, (unsigned)fPageCount);
	return true;
}

void ISCSIVirtualDisk::stop(IOService *provider)
{
	if (fLock) {
		IOLockLock(fLock);
		fOnline = false;
		fMediaChanged = true;
		IOLockUnlock(fLock);
	}
	failAllPending();
	super::stop(provider);
}

void ISCSIVirtualDisk::free()
{
	if (fRequests) {
		fRequests->release();
		fRequests = NULL;
	}
	if (fArena) {
		fArena->release();
		fArena = NULL;
	}
	if (fPageMap) {
		IOFree(fPageMap, (fPageCount + 7) / 8);
		fPageMap = NULL;
	}
	if (fLock) {
		IOLockFree(fLock);
		fLock = NULL;
	}
	super::free();
}

IOReturn ISCSIVirtualDisk::newUserClient(task_t owningTask, void *securityID,
    UInt32 type, OSDictionary *properties, IOUserClient **handler)
{
	if (!handler) {
		return kIOReturnBadArgument;
	}

	ISCSIVirtualDiskUserClient *client = new ISCSIVirtualDiskUserClient;
	if (!client) {
		return kIOReturnNoMemory;
	}
	if (!client->initWithTask(owningTask, securityID, type, properties)) {
		client->release();
		return kIOReturnNotPrivileged;
	}
	if (!client->attach(this)) {
		client->release();
		return kIOReturnError;
	}
	if (!client->start(this)) {
		client->detach(this);
		client->release();
		return kIOReturnError;
	}

	*handler = client;
	return kIOReturnSuccess;
}

/* --- User client and geometry ------------------------------------------- */

bool ISCSIVirtualDisk::attachUserClient(ISCSIVirtualDiskUserClient *client)
{
	bool accepted = false;

	IOLockLock(fLock);
	if (!fClient) {
		fClient = client;
		accepted = true;
	}
	IOLockUnlock(fLock);
	return accepted;
}

void ISCSIVirtualDisk::detachUserClient(ISCSIVirtualDiskUserClient *client)
{
	bool changed = false;

	IOLockLock(fLock);
	if (fClient == client) {
		fClient = NULL;
		if (fOnline) {
			fOnline = false;
			fMediaChanged = true;
			changed = true;
		}
	}
	IOLockUnlock(fLock);

	if (changed) {
		messageClients(kIOMessageMediaStateHasChanged,
		    (void *)(uintptr_t)kIOMediaStateOffline);
	}
	failAllPending();
}

IOReturn ISCSIVirtualDisk::setGeometry(UInt32 blockSize, UInt64 blockCount)
{
	if (blockSize < 512 || blockSize > 65536 ||
	    (blockSize & (blockSize - 1)) != 0 || blockCount == 0) {
		return kIOReturnBadArgument;
	}

	IOLockLock(fLock);
	fBlockSize = blockSize;
	fBlockCount = blockCount;
	fMediaChanged = true;
	IOLockUnlock(fLock);

	setProperty("Block Size", (unsigned long long)blockSize, 32);
	messageClients(kIOMessageMediaStateHasChanged,
	    (void *)(uintptr_t)kIOMediaStateOnline);
	IOLog("ISCSIVirtualDisk: geometry %u byte blocks x %llu\n",
	    (unsigned)blockSize, (unsigned long long)blockCount);
	return kIOReturnSuccess;
}

IOReturn ISCSIVirtualDisk::setOnline(bool online)
{
	bool changed;

	IOLockLock(fLock);
	changed = (fOnline != online);
	fOnline = online;
	fMediaChanged = true;
	IOLockUnlock(fLock);

	if (changed) {
		messageClients(kIOMessageMediaStateHasChanged,
		    (void *)(uintptr_t)(online ? kIOMediaStateOnline
		                              : kIOMediaStateOffline));
		IOLog("ISCSIVirtualDisk: media %s\n", online ? "online" : "offline");
	}
	if (!online) {
		failAllPending();
	}
	return kIOReturnSuccess;
}

IOReturn ISCSIVirtualDisk::getConfig(UInt64 *output, UInt32 *outputCount)
{
	if (!output || !outputCount || *outputCount < 4) {
		return kIOReturnBadArgument;
	}
	output[0] = kISCSIVDAbiVersion;
	output[1] = fArenaBytes;
	output[2] = kISCSIVDPageGranule;
	output[3] = kISCSIVDMaxRequestBytes;
	*outputCount = 4;
	return kIOReturnSuccess;
}

IOMemoryDescriptor *ISCSIVirtualDisk::copyMemoryForType(UInt32 type)
{
	if (type == kISCSIVDMemoryArena && fArena) {
		fArena->retain();
		return fArena;
	}
	if (type == kISCSIVDMemoryRequests && fRequests) {
		return fRequests->getMemoryDescriptor();
	}
	return NULL;
}

/* --- Arena allocator ---------------------------------------------------- */

void *ISCSIVirtualDisk::arenaBaseLocked(void) const
{
	return fArena ? fArena->getBytesNoCopy() : NULL;
}

UInt32 ISCSIVirtualDisk::arenaAllocLocked(UInt32 bytes)
{
	if (!bytes || !fPageMap || bytes > fArenaBytes) {
		return kInvalidArenaOffset;
	}

	UInt32 pages = (bytes + kISCSIVDPageGranule - 1) / kISCSIVDPageGranule;
	UInt32 run = 0;
	UInt32 runStart = 0;

	for (UInt32 page = 0; page < fPageCount; page++) {
		bool used = (fPageMap[page >> 3] >> (page & 7)) & 1;
		if (used) {
			run = 0;
			continue;
		}
		if (run == 0) {
			runStart = page;
		}
		run++;
		if (run == pages) {
			for (UInt32 mark = runStart; mark < runStart + pages; mark++) {
				fPageMap[mark >> 3] |= (uint8_t)(1u << (mark & 7));
			}
			return runStart * kISCSIVDPageGranule;
		}
	}
	return kInvalidArenaOffset;
}

void ISCSIVirtualDisk::arenaFreeLocked(UInt32 offset, UInt32 bytes)
{
	if (!fPageMap || offset == kInvalidArenaOffset || bytes == 0) {
		return;
	}
	UInt32 firstPage = offset / kISCSIVDPageGranule;
	UInt32 pages = (bytes + kISCSIVDPageGranule - 1) / kISCSIVDPageGranule;
	if (firstPage >= fPageCount) {
		return;
	}
	if (pages > fPageCount - firstPage) {
		pages = fPageCount - firstPage;
	}
	for (UInt32 page = firstPage; page < firstPage + pages; page++) {
		fPageMap[page >> 3] &= (uint8_t)~(1u << (page & 7));
	}
}

ISCSIPendingIO *ISCSIVirtualDisk::pendingForTokenLocked(UInt32 token)
{
	UInt32 slot = token & 0xffu;
	if (slot >= kISCSIVDPendingIO) {
		return NULL;
	}
	ISCSIPendingIO *io = &fPending[slot];
	if (!io->inUse || io->token != token) {
		return NULL;
	}
	return io;
}

void ISCSIVirtualDisk::failAllPending(void)
{
	IOStorageCompletion completions[kISCSIVDPendingIO];
	IOReturn statuses[kISCSIVDPendingIO];
	UInt32 count = 0;

	if (!fLock) {
		return;
	}

	IOLockLock(fLock);
	for (UInt32 i = 0; i < kISCSIVDPendingIO; i++) {
		ISCSIPendingIO *io = &fPending[i];
		if (!io->inUse) {
			continue;
		}
		for (UInt32 c = 0; c < io->chunkCount; c++) {
			arenaFreeLocked(io->chunkArena[c], io->chunkLength[c]);
		}
		completions[count] = io->completion;
		statuses[count] = kIOReturnOffline;
		count++;
		io->inUse = false;
	}
	IOLockUnlock(fLock);

	for (UInt32 i = 0; i < count; i++) {
		IOStorage::complete(&completions[i], statuses[i], 0);
	}
}

/* --- IOBlockStorageDevice ----------------------------------------------- */

IOReturn ISCSIVirtualDisk::doEjectMedia(void)
{
	return kIOReturnUnsupported;
}

IOReturn ISCSIVirtualDisk::doFormatMedia(UInt64 byteCapacity)
{
	(void)byteCapacity;
	return kIOReturnUnsupported;
}

UInt32 ISCSIVirtualDisk::doGetFormatCapacities(UInt64 *capacities,
    UInt32 capacitiesMaxCount) const
{
	(void)capacities;
	(void)capacitiesMaxCount;
	return 0;
}

IOReturn ISCSIVirtualDisk::doSynchronizeCache(void)
{
	return kIOReturnSuccess;
}

char *ISCSIVirtualDisk::getVendorString(void)
{
	return (char *)"iscsi-rs";
}

char *ISCSIVirtualDisk::getProductString(void)
{
	return (char *)"Virtual iSCSI Disk";
}

char *ISCSIVirtualDisk::getRevisionString(void)
{
	return (char *)"1.0";
}

char *ISCSIVirtualDisk::getAdditionalDeviceInfoString(void)
{
	return (char *)"";
}

IOReturn ISCSIVirtualDisk::reportBlockSize(UInt64 *blockSize)
{
	if (!blockSize) {
		return kIOReturnBadArgument;
	}
	UInt32 size;
	IOLockLock(fLock);
	size = fBlockSize;
	IOLockUnlock(fLock);
	*blockSize = size ? size : 512;
	return kIOReturnSuccess;
}

IOReturn ISCSIVirtualDisk::reportEjectability(bool *isEjectable)
{
	if (!isEjectable) {
		return kIOReturnBadArgument;
	}
	*isEjectable = false;
	return kIOReturnSuccess;
}

IOReturn ISCSIVirtualDisk::reportMaxValidBlock(UInt64 *maxBlock)
{
	if (!maxBlock) {
		return kIOReturnBadArgument;
	}
	IOLockLock(fLock);
	*maxBlock = fBlockCount ? fBlockCount - 1 : 0;
	IOLockUnlock(fLock);
	return kIOReturnSuccess;
}

IOReturn ISCSIVirtualDisk::reportMediaState(bool *mediaPresent,
    bool *changedState)
{
	if (!mediaPresent) {
		return kIOReturnBadArgument;
	}

	IOLockLock(fLock);
	*mediaPresent = fOnline && fBlockCount != 0;
	if (changedState) {
		*changedState = fMediaChanged;
		fMediaChanged = false;
	}
	IOLockUnlock(fLock);
	return kIOReturnSuccess;
}

IOReturn ISCSIVirtualDisk::reportRemovability(bool *isRemovable)
{
	if (!isRemovable) {
		return kIOReturnBadArgument;
	}
	*isRemovable = false;
	return kIOReturnSuccess;
}

IOReturn ISCSIVirtualDisk::reportWriteProtection(bool *isWriteProtected)
{
	if (!isWriteProtected) {
		return kIOReturnBadArgument;
	}
	/* First release is read-only: never write to the remote LUN. */
	*isWriteProtected = true;
	return kIOReturnSuccess;
}

IOReturn ISCSIVirtualDisk::doAsyncReadWrite(IOMemoryDescriptor *buffer,
    UInt64 block, UInt64 nblks, IOStorageAttributes *attributes,
    IOStorageCompletion *completion)
{
	(void)attributes;

	if (!buffer || !completion) {
		return kIOReturnBadArgument;
	}
	IOStorageCompletion done = *completion;

	IOLockLock(fLock);
	bool ready = fOnline && fClient != NULL && fBlockSize != 0;
	UInt32 blockSize = fBlockSize;
	UInt64 blockCount = fBlockCount;
	IOLockUnlock(fLock);

	if (!ready) {
		IOStorage::complete(&done, kIOReturnNoMedia, 0);
		return kIOReturnSuccess;
	}
	if (buffer->getDirection() & kIODirectionOut) {
		IOStorage::complete(&done, kIOReturnNotWritable, 0);
		return kIOReturnSuccess;
	}
	if (nblks == 0) {
		IOStorage::complete(&done, kIOReturnSuccess, 0);
		return kIOReturnSuccess;
	}
	if (block >= blockCount || nblks > blockCount - block) {
		IOStorage::complete(&done, kIOReturnBadArgument, 0);
		return kIOReturnSuccess;
	}

	UInt64 length64 = nblks * (UInt64)blockSize;
	if (length64 > (UInt64)kISCSIVDMaxRequestBytes) {
		IOStorage::complete(&done, kIOReturnBadArgument, 0);
		return kIOReturnSuccess;
	}
	UInt32 length = (UInt32)length64;

	UInt32 chunkBytes = (kISCSIVDChunkBytes / blockSize) * blockSize;
	if (chunkBytes == 0) {
		chunkBytes = blockSize;
	}
	UInt32 chunkCount = (length + chunkBytes - 1) / chunkBytes;

	IOLockLock(fLock);

	int slot = -1;
	for (UInt32 i = 0; i < kISCSIVDPendingIO; i++) {
		if (!fPending[i].inUse) {
			slot = (int)i;
			break;
		}
	}	if (slot < 0) {
		IOLockUnlock(fLock);
		IOStorage::complete(&done, kIOReturnNoResources, 0);
		return kIOReturnSuccess;
	}

	ISCSIPendingIO *io = &fPending[slot];
	bzero(io, sizeof(*io));
	fGeneration = (fGeneration + 1) & 0x00ffffffu;
	if (fGeneration == 0) {
		fGeneration = 1;
	}
	io->inUse = true;
	io->generation = fGeneration;
	io->token = (fGeneration << 8) | (UInt32)slot;
	io->chunkBytes = chunkBytes;
	io->chunkCount = chunkCount;
	io->buffer = buffer;
	io->bufferOffset = 0;
	io->status = kIOReturnSuccess;
	io->completion = done;
	for (UInt32 c = 0; c < chunkCount; c++) {
		io->chunkArena[c] = kInvalidArenaOffset;
	}

	bool queued = true;
	for (UInt32 c = 0; c < chunkCount; c++) {
		UInt32 offset = c * chunkBytes;
		UInt32 chunkLength = (length - offset > chunkBytes)
		    ? chunkBytes : (length - offset);
		UInt32 arenaOffset = arenaAllocLocked(chunkLength);
		if (arenaOffset == kInvalidArenaOffset) {
			queued = false;
			break;
		}
		io->chunkLength[c] = chunkLength;
		io->chunkArena[c] = arenaOffset;

		iscsi_vd_request request;
		bzero(&request, sizeof(request));
		request.token = io->token;
		request.chunk = c;
		request.length = chunkLength;
		request.block_size = blockSize;
		request.lba = block + (offset / blockSize);
		request.arena_offset = arenaOffset;
		request.direction = kISCSIVDTransferRead;

		if (!fRequests->enqueue(&request, sizeof(request))) {
			arenaFreeLocked(arenaOffset, chunkLength);
			io->chunkArena[c] = kInvalidArenaOffset;
			queued = false;
			break;
		}
	}

	if (!queued) {
		for (UInt32 c = 0; c < chunkCount; c++) {
			if (io->chunkArena[c] != kInvalidArenaOffset) {
				arenaFreeLocked(io->chunkArena[c], io->chunkLength[c]);
				io->chunkArena[c] = kInvalidArenaOffset;
			}
		}
		io->inUse = false;
		IOLockUnlock(fLock);
		IOStorage::complete(&done, kIOReturnNoResources, 0);
		return kIOReturnSuccess;
	}

	IOLockUnlock(fLock);
	return kIOReturnSuccess;
}

IOReturn ISCSIVirtualDisk::completeChunk(UInt32 token, UInt32 chunk,
    UInt32 status, UInt32 bytes)
{
	IOMemoryDescriptor *buffer = NULL;
	UInt64 bufferOffset = 0;
	void *source = NULL;
	UInt32 length = 0;
	bool copy = false;

	(void)bytes;

	IOLockLock(fLock);
	ISCSIPendingIO *io = pendingForTokenLocked(token);
	if (!io || chunk >= io->chunkCount || io->chunkDone[chunk]) {
		IOLockUnlock(fLock);
		return kIOReturnBadArgument;
	}
	io->chunkDone[chunk] = true;
	io->chunksDone++;
	if (status != kISCSIVDStatusSuccess) {
		io->status = kIOReturnError;
	} else {
		copy = true;
		buffer = io->buffer;
		bufferOffset = io->bufferOffset + (UInt64)chunk * io->chunkBytes;
		length = io->chunkLength[chunk];
		source = (uint8_t *)arenaBaseLocked() + io->chunkArena[chunk];
	}
	IOLockUnlock(fLock);

	IOReturn copyStatus = kIOReturnSuccess;
	if (copy) {
		copyStatus = copyIntoDescriptor(buffer, bufferOffset, source, length);
	}

	IOStorageCompletion completion;
	bool finished = false;
	IOReturn finalStatus = kIOReturnSuccess;
	UInt32 finalBytes = 0;

	IOLockLock(fLock);
	io = pendingForTokenLocked(token);
	if (!io) {
		IOLockUnlock(fLock);
		return kIOReturnNotFound;
	}
	if (copy) {
		if (copyStatus == kIOReturnSuccess) {
			io->bytesDone += length;
		} else {
			io->status = copyStatus;
		}
	}
	arenaFreeLocked(io->chunkArena[chunk], io->chunkLength[chunk]);
	io->chunkArena[chunk] = kInvalidArenaOffset;

	if (io->chunksDone == io->chunkCount) {
		finished = true;
		finalStatus = io->status;
		finalBytes = io->bytesDone;
		completion = io->completion;
		io->inUse = false;
	}
	IOLockUnlock(fLock);

	if (finished) {
		IOStorage::complete(&completion, finalStatus, finalBytes);
	}
	return kIOReturnSuccess;
}

IOReturn ISCSIVirtualDisk::doUnmap(IOBlockStorageDeviceExtent *extents,
    UInt32 extentsCount, IOStorageUnmapOptions options)
{
	(void)extents;
	(void)extentsCount;
	(void)options;
	return kIOReturnSuccess;
}

IOReturn ISCSIVirtualDisk::doSynchronize(UInt64 block, UInt64 nblks,
    IOStorageSynchronizeOptions options)
{
	(void)block;
	(void)nblks;
	(void)options;
	return kIOReturnSuccess;
}

IOReturn ISCSIVirtualDisk::requestIdle(void)
{
	return kIOReturnSuccess;
}

IOReturn ISCSIVirtualDisk::getWriteCacheState(bool *enabled)
{
	if (!enabled) {
		return kIOReturnBadArgument;
	}
	*enabled = false;
	return kIOReturnSuccess;
}

IOReturn ISCSIVirtualDisk::setWriteCacheState(bool enabled)
{
	return enabled ? kIOReturnUnsupported : kIOReturnSuccess;
}
