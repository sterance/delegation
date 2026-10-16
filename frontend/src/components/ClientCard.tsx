import { useState } from "react";
import DeleteOutlinedIcon from "@mui/icons-material/DeleteOutlined";
import ExpandMoreIcon from "@mui/icons-material/ExpandMore";
import Box from "@mui/material/Box";
import Card from "@mui/material/Card";
import CardContent from "@mui/material/CardContent";
import Collapse from "@mui/material/Collapse";
import Divider from "@mui/material/Divider";
import IconButton from "@mui/material/IconButton";
import Stack from "@mui/material/Stack";
import Tooltip from "@mui/material/Tooltip";
import Typography from "@mui/material/Typography";
import { MAX_FRIENDLY_NAME_LENGTH, type Client, type HardwareSpecs } from "../api";
import { MISSING, formatCpu, formatDisk, formatGpu, formatMemory, specsSummary } from "../format";
import InlineTextEdit from "./InlineTextEdit";

const timestampFormat = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "medium" });

function formatTimestamp(iso: string | null): string {
  if (!iso) return "Never";
  const date = new Date(iso);
  return Number.isNaN(date.getTime()) ? iso : timestampFormat.format(date);
}

function StatusDot({ connected }: { connected: boolean }) {
  const label = connected ? "Connected" : "Disconnected";
  return (
    <Box
      component="span"
      role="img"
      aria-label={label}
      title={label}
      sx={{
        width: 10,
        height: 10,
        borderRadius: "50%",
        flexShrink: 0,
        bgcolor: connected ? "success.main" : "error.main",
      }}
    />
  );
}

function DetailRow({ label, value, title }: { label: string; value: string; title?: string }) {
  return (
    <Stack
      direction="row"
      spacing={2}
      sx={{ justifyContent: "space-between" }}
    >
      <Typography
        variant="body2"
        color="text.secondary"
        sx={{ flexShrink: 0 }}
      >
        {label}
      </Typography>
      <Typography
        variant="body2"
        title={title}
        sx={{ textAlign: "right", minWidth: 0, overflowWrap: "anywhere" }}
      >
        {value}
      </Typography>
    </Stack>
  );
}

function Timestamp({ label, iso }: { label: string; iso: string | null }) {
  return (
    <DetailRow
      label={label}
      value={formatTimestamp(iso)}
      title={iso ?? undefined}
    />
  );
}

function Hardware({ specs }: { specs: HardwareSpecs | null }) {
  if (!specs) {
    return (
      <Typography
        variant="body2"
        color="text.secondary"
      >
        No hardware info yet
      </Typography>
    );
  }
  return (
    <>
      <DetailRow
        label="CPU"
        value={formatCpu(specs.cpu)}
        title={specs.cpu?.model ?? undefined}
      />
      <DetailRow
        label="RAM"
        value={formatMemory(specs.memory)}
      />
      {specs.gpus.length === 0 ? (
        <DetailRow
          label="GPU"
          value={MISSING}
        />
      ) : (
        specs.gpus.map((gpu, i) => (
          <DetailRow
            key={i}
            label="GPU"
            value={formatGpu(gpu)}
            title={gpu.model ?? undefined}
          />
        ))
      )}
      {specs.disks.length === 0 ? (
        <DetailRow
          label="Disk"
          value={MISSING}
        />
      ) : (
        specs.disks.map((disk, i) => (
          <DetailRow
            key={i}
            label="Disk"
            value={formatDisk(disk)}
          />
        ))
      )}
    </>
  );
}

interface Props {
  client: Client;
  onRename: (friendlyName: string) => Promise<void>;
  onDelete: () => void;
}

export default function ClientCard({ client, onRename, onDelete }: Props) {
  const [expanded, setExpanded] = useState(false);
  const detailsId = `client-${client.client_id}-details`;
  const summary = specsSummary(client.specs);

  return (
    <Card
      elevation={expanded ? 4 : 2}
      sx={{ transition: (theme) => theme.transitions.create("box-shadow") }}
    >
      <CardContent sx={{ display: "flex", alignItems: "center", gap: 2, "&:last-child": { pb: 2 } }}>
        <StatusDot connected={client.connected} />
        <Box sx={{ flexGrow: 1, minWidth: 0 }}>
          <InlineTextEdit
            value={client.friendly_name}
            onSave={onRename}
            label="Client name"
            placeholder={`Blank resets to ${client.hostname}`}
            maxLength={MAX_FRIENDLY_NAME_LENGTH}
            typographyProps={{ variant: "body1", sx: { fontWeight: 500 } }}
          />
          {summary && (
            <Typography
              variant="body2"
              color="text.secondary"
              noWrap
            >
              {summary}
            </Typography>
          )}
        </Box>
        <Tooltip title="Delete client">
          <IconButton
            onClick={onDelete}
            aria-label={`Delete ${client.friendly_name}`}
            size="small"
          >
            <DeleteOutlinedIcon />
          </IconButton>
        </Tooltip>
        <IconButton
          onClick={() => setExpanded((v) => !v)}
          aria-expanded={expanded}
          aria-controls={detailsId}
          aria-label={expanded ? "Hide details" : "Show details"}
          size="small"
        >
          <ExpandMoreIcon
            sx={{
              transform: expanded ? "rotate(180deg)" : "none",
              transition: (theme) => theme.transitions.create("transform"),
            }}
          />
        </IconButton>
      </CardContent>
      <Collapse
        in={expanded}
        timeout="auto"
        unmountOnExit
        id={detailsId}
      >
        <CardContent sx={{ pt: 0, "&:last-child": { pb: 2 } }}>
          <Stack spacing={0.5}>
            <Hardware specs={client.specs} />
            <DetailRow
              label="OS"
              value={client.os ?? MISSING}
            />
            <DetailRow
              label="Client version"
              value={client.client_version ?? MISSING}
            />
          </Stack>
          <Divider sx={{ my: 1.5 }} />
          <Stack spacing={0.5}>
            <DetailRow
              label="Hostname"
              value={client.hostname}
            />
            <DetailRow
              label="Client ID"
              value={client.client_id}
            />
            <Timestamp
              label="Enrolled"
              iso={client.enrolled_at}
            />
            <Timestamp
              label="Last seen"
              iso={client.last_seen_at}
            />
          </Stack>
        </CardContent>
      </Collapse>
    </Card>
  );
}
