import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '../ui/select';
import { useMutation, useQueryClient } from '@tanstack/react-query';
import { useTRPC } from '@/providers/query-provider';
import { useState, type FormEvent } from 'react';
import { Button } from '../ui/button';
import { Input } from '../ui/input';
import { Label } from '../ui/label';
import { toast } from 'sonner';

type Security = 'tls' | 'starttls' | 'none';

const defaultPort = {
  imap: { tls: 993, starttls: 143, none: 143 },
  smtp: { tls: 465, starttls: 587, none: 25 },
} as const;

export function ImapConnectForm({ onDone }: { onDone?: () => void }) {
  const trpc = useTRPC();
  const queryClient = useQueryClient();
  const [email, setEmail] = useState('');
  const [name, setName] = useState('');
  const [password, setPassword] = useState('');
  const [username, setUsername] = useState('');
  const [imapHost, setImapHost] = useState('');
  const [imapPort, setImapPort] = useState<number>(993);
  const [imapSecurity, setImapSecurity] = useState<Security>('tls');
  const [smtpHost, setSmtpHost] = useState('');
  const [smtpPort, setSmtpPort] = useState<number>(465);
  const [smtpSecurity, setSmtpSecurity] = useState<Security>('tls');

  const { mutateAsync: addImap, isPending } = useMutation(
    trpc.connections.addImap.mutationOptions(),
  );

  const onEmailBlur = () => {
    const domain = email.split('@')[1];
    if (!domain) return;
    if (!imapHost) setImapHost(`imap.${domain}`);
    if (!smtpHost) setSmtpHost(`smtp.${domain}`);
  };

  const onSubmit = async (e: FormEvent) => {
    e.preventDefault();
    await toast.promise(
      addImap({
        email,
        name,
        password,
        username: username || email,
        imapHost,
        imapPort,
        imapSecurity,
        smtpHost,
        smtpPort,
        smtpSecurity,
      }),
      {
        loading: 'Checking IMAP and SMTP settings...',
        success: () => {
          queryClient.invalidateQueries({ queryKey: trpc.connections.list.queryKey() });
          onDone?.();
          return `${email} connected. Mail will appear in a moment.`;
        },
        error: (err) => err?.message ?? 'Could not connect',
      },
    );
  };

  return (
    <form onSubmit={onSubmit} className="mt-4 grid gap-3">
      <div className="grid grid-cols-2 gap-3">
        <Field label="Email">
          <Input
            type="email"
            required
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            onBlur={onEmailBlur}
            placeholder="you@example.com"
          />
        </Field>
        <Field label="Display name">
          <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="Jan Kowalski" />
        </Field>
        <Field label="Username (optional)">
          <Input value={username} onChange={(e) => setUsername(e.target.value)} placeholder={email || 'same as email'} />
        </Field>
        <Field label="Password">
          <Input type="password" required value={password} onChange={(e) => setPassword(e.target.value)} />
        </Field>
      </div>
      <ServerRow
        title="IMAP"
        host={imapHost}
        port={imapPort}
        security={imapSecurity}
        onHost={setImapHost}
        onPort={setImapPort}
        onSecurity={(s) => {
          setImapSecurity(s);
          setImapPort(defaultPort.imap[s]);
        }}
      />
      <ServerRow
        title="SMTP"
        host={smtpHost}
        port={smtpPort}
        security={smtpSecurity}
        onHost={setSmtpHost}
        onPort={setSmtpPort}
        onSecurity={(s) => {
          setSmtpSecurity(s);
          setSmtpPort(defaultPort.smtp[s]);
        }}
      />
      <Button type="submit" disabled={isPending}>
        {isPending ? 'Connecting...' : 'Connect mailbox'}
      </Button>
    </form>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="grid gap-1.5">
      <Label className="text-xs">{label}</Label>
      {children}
    </div>
  );
}

function ServerRow(props: {
  title: string;
  host: string;
  port: number;
  security: Security;
  onHost: (v: string) => void;
  onPort: (v: number) => void;
  onSecurity: (v: Security) => void;
}) {
  return (
    <div className="grid grid-cols-[1fr_90px_130px] gap-3">
      <Field label={`${props.title} server`}>
        <Input required value={props.host} onChange={(e) => props.onHost(e.target.value)} />
      </Field>
      <Field label="Port">
        <Input
          type="number"
          required
          value={props.port}
          onChange={(e) => props.onPort(Number(e.target.value))}
        />
      </Field>
      <Field label="Security">
        <Select value={props.security} onValueChange={(v) => props.onSecurity(v as Security)}>
          <SelectTrigger>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="tls">SSL/TLS</SelectItem>
            <SelectItem value="starttls">STARTTLS</SelectItem>
            <SelectItem value="none">None</SelectItem>
          </SelectContent>
        </Select>
      </Field>
    </div>
  );
}
