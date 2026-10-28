import { formatName } from './format';
import type { User } from './types';
import Card from './Card.tsrx';

type Props = { users: User[]; showBio: boolean };

const theme = <style>
  .base { color: red; }
</style>;

export function Greeting({ name }: { name?: string }) @{
  const message = name ? `Hello, ${formatName(name)}` : 'Hello, stranger';

  <>
    <div class="greeting">
      <p>{message}</p>
    </div>
    <style>
      .greeting { padding: 1rem; }
      .greeting p:hover { color: blue; }
    </style>
  </>
}

function UserList({ users, showBio }: Props) @{
  const total = users.length;

  <ul>
    @for (const user of users; key user.id index i) {
      const short_bio = user.bio.slice(0, 140);
      <li>
        <Card title={user.name} />
        @if (showBio && user.bio) {
          <p>{short_bio} {i}</p>
        } @else {
          <p>No bio</p>
        }
      </li>
    } @empty {
      <li>Nobody here ({total})</li>
    }
  </ul>
}

export const Total = ({ items }: { items: number[] }) => @{
  const subtotal = items.reduce((sum, item) => sum + item, 0);

  <p>Save: ${(subtotal * 0.9).toFixed(2)} https://example.com</p>
}

export function pad(n: number): string {
  return String(n).padStart(2, '0');
}
