/*
 * Shared ABI between the ISCSIVirtualDisk kext and the userspace iscsi-blockd
 * daemon. Keep this file in sync with crates/iscsi-blockd/src/abi.rs.
 *
 * The daemon owns the iSCSI protocol; the kext only moves blocks. Requests are
 * queued kernel -> user through an IOSharedDataQueue, completions come back
 * through external method kISCSIVDMethodCompleteIO, and payloads live in one
 * shared arena buffer mapped into both address spaces.
 */
#ifndef ISCSI_VD_PROTOCOL_H
#define ISCSI_VD_PROTOCOL_H

#include <stdint.h>

#define kISCSIVDAbiVersion        1u

/* Concurrent storage requests the kext tracks. */
#define kISCSIVDPendingIO         32
/* Chunks per storage request. Upper bound on a single request is the product. */
#define kISCSIVDChunksPerIO       32
#define kISCSIVDChunkBytes        (1024u * 1024u)
#define kISCSIVDMaxRequestBytes   (kISCSIVDChunksPerIO * kISCSIVDChunkBytes)
#define kISCSIVDQueueEntries      1024
/* Arena allocation granularity. Must match PAGE_SIZE on Apple silicon. */
#define kISCSIVDPageGranule       16384u
#define kISCSIVDDefaultArenaBytes (64ull * 1024ull * 1024ull)

/* External method selectors (IOConnectCallScalarMethod). */
enum {
	kISCSIVDMethodSetGeometry = 0,
	kISCSIVDMethodCompleteIO  = 1,
	kISCSIVDMethodSetOnline   = 2,
	kISCSIVDMethodGetConfig   = 3,
	kISCSIVDMethodCount       = 4
};

/* IOConnectMapMemory64 memory types. */
enum {
	kISCSIVDMemoryArena    = 0,
	kISCSIVDMemoryRequests = 1
};

/* Transfer direction as seen from the initiator (macOS). */
enum {
	kISCSIVDTransferRead  = 0,
	kISCSIVDTransferWrite = 1
};

/* Completion status values understood by the kext. */
enum {
	kISCSIVDStatusSuccess = 0,
	kISCSIVDStatusFailure = 1
};

typedef struct iscsi_vd_request {
	uint32_t token;       /* pending IO slot | generation << 8 */
	uint32_t chunk;       /* chunk index inside the storage request */
	uint32_t length;      /* payload bytes for this chunk */
	uint32_t block_size;  /* logical block size in bytes */
	uint64_t lba;         /* first logical block of this chunk */
	uint64_t arena_offset;/* byte offset inside the shared arena */
	uint32_t direction;   /* kISCSIVDTransfer* */
	uint32_t reserved;
} iscsi_vd_request;

#endif /* ISCSI_VD_PROTOCOL_H */
